//! `lsp` 工具：14 个动作的严格解析、动作级门禁与同步执行。
//!
//! 门禁分两层且彼此独立：工具定义按 `Access` 裁剪（只读会话只暴露只读动作），
//! `validate_access` 与 [`run`] 在参数解析后再次校验，隐藏 schema 永远不是
//! 授权边界。`run` 返回 pp 工具结果（`ok`/`content`/`details`）；取消与清理
//! 失败保留 typed error 向上传播，普通动作失败用 `ok:false` 的完整结果说明。
//!
//! 所有编辑仅用于建议；服务器 applyEdit 一律拒绝，工具从不提交文件修改。

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use async_openai::types::chat::FunctionObject;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    cancellation::{CancellationToken, Cancelled, CleanupError, check},
    events::AgentEvent,
    lsp::{
        Registry, ReportStatus, adapters,
        client::{self, Client},
        config::ServerDefinition,
        diagnostics, edits,
        transport::{Failure, ServerRequest, UNAUTHORIZED_EDIT},
        types::{
            Command, Diagnostic, DocumentSymbol, Location, LocationLink, SymbolInformation,
            TextEdit, WorkspaceEdit,
        },
        uri,
    },
    tools::{Tools, approval::Tier},
};

/// 只读会话（Plan/Review、只读子 Agent）允许的动作全集。
const READONLY_ACTIONS: &[&str] = &[
    "diagnostics",
    "definition",
    "type_definition",
    "implementation",
    "references",
    "hover",
    "symbols",
    "status",
    "capabilities",
    "rename",
    "rename_file",
    "code_actions",
    "format",
];

/// 需要先等项目索引完成的导航动作。
const PROJECT_INDEXED_ACTIONS: &[&str] = &[
    "definition",
    "type_definition",
    "implementation",
    "references",
    "rename",
    "hover",
];

/// 显式 `symbol` 是必需的动作（project-aware 服务器上）。
const SYMBOL_REQUIRED_ACTIONS: &[&str] = &["definition", "references", "rename"];

/// 位置上下文行数（上下各一行）。
const CONTEXT_LINES: u32 = 1;
/// `references` 带上下文展示的条数上限。
const REFERENCE_CONTEXT_LIMIT: usize = 50;
/// project-aware 服务器只返回声明时额外查询次数与间隔。
const REFERENCES_RETRY_COUNT: u32 = 2;
const REFERENCES_RETRY_DELAY: Duration = Duration::from_millis(250);
/// 工作区符号结果上限。
const WORKSPACE_SYMBOL_LIMIT: usize = 200;
/// 一次目录重命名最多枚举的普通文件数。
const MAX_RENAME_PAIRS: usize = 1000;
/// `code_actions` 为快速修复提供的诊断上下文等待预算。
const CODE_ACTION_DIAGNOSTICS_WAIT: Duration = Duration::from_secs(3);
/// 无显式 `timeout` 时的调用预算下限。
const MIN_CALL_BUDGET: u64 = 20;

/// 工具的动作级访问模式；`ReadOnly` 在定义与执行两层同时生效。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    Execute,
    ReadOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Diagnostics,
    Definition,
    References,
    Hover,
    Symbols,
    Rename,
    RenameFile,
    CodeActions,
    TypeDefinition,
    Implementation,
    Status,
    Reload,
    Capabilities,
    Format,
}

impl Action {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "diagnostics" => Self::Diagnostics,
            "definition" => Self::Definition,
            "references" => Self::References,
            "hover" => Self::Hover,
            "symbols" => Self::Symbols,
            "rename" => Self::Rename,
            "rename_file" => Self::RenameFile,
            "code_actions" => Self::CodeActions,
            "type_definition" => Self::TypeDefinition,
            "implementation" => Self::Implementation,
            "status" => Self::Status,
            "reload" => Self::Reload,
            "capabilities" => Self::Capabilities,
            "format" => Self::Format,
            other => bail!("unknown lsp action `{other}`"),
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Diagnostics => "diagnostics",
            Self::Definition => "definition",
            Self::References => "references",
            Self::Hover => "hover",
            Self::Symbols => "symbols",
            Self::Rename => "rename",
            Self::RenameFile => "rename_file",
            Self::CodeActions => "code_actions",
            Self::TypeDefinition => "type_definition",
            Self::Implementation => "implementation",
            Self::Status => "status",
            Self::Reload => "reload",
            Self::Capabilities => "capabilities",
            Self::Format => "format",
        }
    }

    fn is_readonly(self) -> bool {
        READONLY_ACTIONS.contains(&self.as_str())
    }
}

/// 严格的工具参数；未知字段拒绝，`line`/`timeout` 在解析阶段校验。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawParams {
    action: String,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    new_name: Option<String>,
    #[serde(default)]
    apply: Option<bool>,
    #[serde(default)]
    timeout: Option<f64>,
    #[serde(default)]
    selection: Option<String>,
}

#[derive(Debug)]
struct Params {
    action: Action,
    file: Option<String>,
    line: Option<u32>,
    symbol: Option<String>,
    query: Option<String>,
    new_name: Option<String>,
    timeout: Option<f64>,
    selection: Option<String>,
}

impl Params {
    fn parse(arguments: &str) -> Result<Self> {
        let raw: RawParams = serde_json::from_str(arguments).context("invalid lsp arguments")?;
        let action = Action::parse(&raw.action)?;
        ensure!(
            raw.apply != Some(true),
            "LSP only returns information; apply=true is unsupported. Use edit/write/bash."
        );
        ensure!(
            raw.line != Some(0),
            "line must be a positive 1-based line number"
        );
        if let Some(timeout) = raw.timeout {
            ensure!(
                (5.0..=300.0).contains(&timeout),
                "timeout must be between 5 and 300 seconds"
            );
        }
        Ok(Self {
            action,
            file: raw.file,
            line: raw.line,
            symbol: raw.symbol,
            query: raw.query,
            new_name: raw.new_name,
            timeout: raw.timeout,
            selection: raw.selection,
        })
    }

    fn file(&self) -> Result<&str> {
        self.file
            .as_deref()
            .filter(|file| !file.trim().is_empty())
            .with_context(|| {
                format!(
                    "{} requires `file`; use \"*\" for workspace scope where supported",
                    self.action.as_str()
                )
            })
    }
}

/// `lsp` 工具定义。只读会话只暴露允许的动作，运行时门禁仍然独立生效。
pub(crate) fn definition(read_only: bool) -> FunctionObject {
    let actions: Vec<&str> = if read_only {
        READONLY_ACTIONS.to_vec()
    } else {
        vec![
            "diagnostics",
            "definition",
            "type_definition",
            "implementation",
            "references",
            "hover",
            "symbols",
            "rename",
            "rename_file",
            "code_actions",
            "status",
            "reload",
            "capabilities",
            "format",
        ]
    };
    let mut description = String::from(
        "Query language servers for semantic navigation, diagnostics and modification suggestions. \
         LSP never applies edits or executes server commands. Read files again and use edit/write/bash \
         to implement suggestions. Positions supplied to this tool are 1-based; original LSP ranges \
         returned in suggestions are zero-based UTF-16. Use symbol=name#N for an occurrence. \
         rename and rename_file return suggestions only; format returns formatting suggestions. \
         code_actions query filters by kind; selection resolves a 0-based index or title substring. \
         diagnostics accepts a file or bounded glob; file=* is unsupported: use bash for project checks. \
         reload refreshes configuration and reconnects clients. Servers are not a filesystem sandbox.",
    );
    if read_only {
        description.push_str(
            " This is a read-only session: only the read-only actions are exposed and every other \
             action is refused before any server starts.",
        );
    }
    FunctionObject {
        name: "lsp".into(),
        description: Some(description),
        parameters: Some(json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": actions},
                "file": {"type": "string"},
                "line": {"type": "integer", "minimum": 1},
                "symbol": {"type": "string"},
                "query": {"type": "string"},
                "new_name": {"type": "string"},
                "timeout": {"type": "number", "minimum": 5, "maximum": 300},
                "selection": {"type": "string"}
            },
            "required": ["action"],
            "additionalProperties": false
        })),
        ..Default::default()
    }
}

/// 执行层访问门禁：只读会话必须能证明动作只读，否则在解析后立即拒绝。
pub(crate) fn validate_access(arguments: &str, access: Access) -> Result<()> {
    if access == Access::Execute {
        return Ok(());
    }
    let params = Params::parse(arguments)?;
    enforce_access(&params, access)
}

/// 审批分档：只读动作 Read；写动作（含预览）Write；工作区诊断与无效参数保守处理。
pub(crate) fn approval_tier(arguments: &str) -> Tier {
    let Ok(params) = Params::parse(arguments) else {
        return Tier::Write;
    };
    if params.action == Action::Diagnostics && params.file.is_none() {
        return Tier::Write;
    }
    if params.action.is_readonly() {
        Tier::Read
    } else {
        Tier::Write
    }
}

fn enforce_access(params: &Params, access: Access) -> Result<()> {
    if access == Access::Execute {
        return Ok(());
    }
    ensure!(
        params.action.is_readonly(),
        "LSP action {} is not available in a read-only session",
        params.action.as_str()
    );
    if params.action == Action::Diagnostics {
        ensure!(
            params.file.as_deref() != Some("*"),
            "workspace diagnostics run build commands and are not available in a read-only session"
        );
    }
    Ok(())
}

/// 执行一个 `lsp` 调用，返回 pp 工具结果 JSON。
pub(crate) fn run(
    tools: &Tools,
    arguments: &str,
    _call_id: &str,
    access: Access,
    cancellation: &CancellationToken,
    _emit: &mut dyn FnMut(AgentEvent),
) -> Result<Value> {
    let params = Params::parse(arguments)?;
    enforce_access(&params, access)?;
    check(cancellation)?;
    let registry = tools
        .lsp_registry()
        .context("LSP support is not enabled for this session.")?;
    ensure!(registry.enabled(), "LSP support is disabled.");
    if !matches!(params.action, Action::Status | Action::Reload) {
        // 配置错误是显式调用必须看到的原因；status 把它作为状态展示。
        let _ = registry.config()?;
    }
    let deadline = call_deadline(registry, params.timeout);
    match params.action {
        Action::Status => status(registry),
        Action::Capabilities => capabilities(registry, &params, deadline, cancellation),
        Action::Reload => reload(registry, &params, deadline, cancellation),
        Action::Diagnostics => {
            diagnostics_action(tools, registry, &params, access, deadline, cancellation)
        }
        Action::Definition
        | Action::TypeDefinition
        | Action::Implementation
        | Action::References
        | Action::Hover => navigation(registry, &params, deadline, cancellation),
        Action::Symbols => symbols(registry, &params, deadline, cancellation),
        Action::Rename => rename(tools, registry, &params, deadline, cancellation),
        Action::RenameFile => rename_file(tools, registry, &params, deadline, cancellation),
        Action::CodeActions => code_actions(tools, registry, &params, deadline, cancellation),
        Action::Format => format_suggestions(tools, registry, &params, deadline, cancellation),
    }
}

fn call_deadline(registry: &Registry, timeout: Option<f64>) -> Instant {
    let seconds =
        timeout.unwrap_or_else(|| registry.settings().request_timeout.max(MIN_CALL_BUDGET) as f64);
    Instant::now() + Duration::from_secs_f64(seconds)
}

fn done(action: Action, success: bool, content: String) -> Value {
    json!({
        "ok": success,
        "content": content,
        "details": {"action": action.as_str(), "success": success},
    })
}

fn with_server(mut value: Value, server: &str) -> Value {
    if !server.is_empty() {
        value["details"]["serverName"] = json!(server);
    }
    value
}

fn with_useless(mut value: Value) -> Value {
    value["details"]["useless"] = json!(true);
    value
}

fn with_diagnostics(mut value: Value, diagnostics: Value) -> Value {
    value["diagnostics"] = diagnostics;
    value
}

fn is_control(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<Cancelled>() || cause.is::<CleanupError>())
}

fn failure_error(failure: Failure, what: &str) -> anyhow::Error {
    match failure {
        Failure::Cancelled => anyhow::Error::new(Cancelled),
        Failure::Timeout => anyhow::anyhow!("LSP {what} timed out"),
        Failure::Rpc(code) => anyhow::anyhow!("LSP {what} failed with error code {code}"),
        Failure::Closed => {
            anyhow::anyhow!("LSP {what} failed: the language server connection is closed")
        }
    }
}

/// 显式诊断报告的本 Agent owner：子 Agent 各自持有，不共享 inbox。
fn owner(tools: &Tools) -> &str {
    tools.lsp_owner.id()
}

fn deny_edit(request: ServerRequest) {
    request.respond(json!({
        "applied": false,
        "failureReason": UNAUTHORIZED_EDIT,
    }));
}

/// 只读作用域的 server 回调：一律拒绝 `workspace/applyEdit`。
fn deny_service() -> impl FnMut(ServerRequest) {
    |request: ServerRequest| deny_edit(request)
}

fn project_aware(definition: &ServerDefinition) -> bool {
    !adapters::is_adapter(&definition.name) && !definition.is_linter
}

fn resolve_path(cwd: &Path, file: &str) -> PathBuf {
    let path = Path::new(file);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn display_path(cwd: &Path, path: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(relative) => {
            let text = relative.display().to_string();
            if text.is_empty() {
                ".".to_owned()
            } else {
                text
            }
        }
        Err(_) => path.display().to_string(),
    }
}

fn real_servers_for(registry: &Registry, path: &Path) -> Vec<ServerDefinition> {
    registry.real_servers_for_file(path)
}

fn all_real_servers(registry: &Registry) -> Vec<ServerDefinition> {
    registry
        .discovered()
        .into_iter()
        .filter(|definition| !adapters::is_adapter(&definition.name))
        .collect()
}

/// 项目根的 Rust 标记：只有存在 Cargo.toml/rust-analyzer.toml 祖先才等待索引。
fn has_rust_workspace_ancestor(path: &Path) -> bool {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir.join("Cargo.toml").is_file() || dir.join("rust-analyzer.toml").is_file() {
            return true;
        }
        current = dir.parent();
    }
    false
}

fn wait_for_project(
    client: &Client,
    definition: &ServerDefinition,
    action: Action,
    path: &Path,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<()> {
    if project_aware(definition)
        && PROJECT_INDEXED_ACTIONS.contains(&action.as_str())
        && has_rust_workspace_ancestor(path)
    {
        client.wait_workspace_ready(deadline, cancellation)?;
    }
    Ok(())
}

// =============================================================================
// 位置与符号
// =============================================================================

fn line_text(text: &str, line: u32) -> &str {
    let raw = text
        .split('\n')
        .nth(line.saturating_sub(1) as usize)
        .unwrap_or("");
    raw.strip_suffix('\r').unwrap_or(raw)
}

fn is_bare_identifier(symbol: &str) -> bool {
    let mut chars = symbol.chars();
    match chars.next() {
        Some(first) if first == '$' || first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '$' || ch == '_' || ch.is_ascii_alphanumeric())
}

fn is_identifier_byte(byte: u8) -> bool {
    byte == b'$' || byte == b'_' || byte.is_ascii_alphanumeric()
}

/// 在单行文本上找出全部匹配的字节下标；裸标识符要求词边界。
fn find_matches(line: &str, symbol: &str, case_insensitive: bool) -> Vec<usize> {
    if symbol.is_empty() {
        return Vec::new();
    }
    let (haystack, needle) = if case_insensitive {
        (line.to_ascii_lowercase(), symbol.to_ascii_lowercase())
    } else {
        (line.to_owned(), symbol.to_owned())
    };
    let boundary = is_bare_identifier(symbol);
    let bytes = haystack.as_bytes();
    let mut matches = Vec::new();
    let mut from = 0usize;
    while from + needle.len() <= bytes.len() {
        let Some(offset) = haystack[from..].find(needle.as_str()) else {
            break;
        };
        let index = from + offset;
        let before_ok = index == 0 || !is_identifier_byte(bytes[index - 1]);
        let after = index + needle.len();
        let after_ok = after >= bytes.len() || !is_identifier_byte(bytes[after]);
        if !boundary || (before_ok && after_ok) {
            matches.push(index);
        }
        from = index + needle.len().max(1);
    }
    matches
}

/// `name` 或 `name#N`（N 为 1-based 出现次数，贪婪解析 `#` 后缀）。
fn parse_symbol_spec(spec: &str) -> (String, usize) {
    if let Some((name, digits)) = spec.rsplit_once('#')
        && !name.is_empty()
        && !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        let occurrence = digits.parse::<usize>().unwrap_or(1).max(1);
        return (name.to_owned(), occurrence);
    }
    (spec.to_owned(), 1)
}

/// 解析 `line` 上的 UTF-16 列；无 `symbol` 时取首个非空白字符。
fn resolve_column(text: &str, line: u32, spec: Option<&str>) -> Result<u32> {
    let line = line.max(1);
    let text = line_text(text, line);
    let byte = match spec {
        None => text
            .char_indices()
            .find(|(_, ch)| !ch.is_whitespace())
            .map_or(0, |(index, _)| index),
        Some(spec) => {
            let (symbol, occurrence) = parse_symbol_spec(spec);
            ensure!(!symbol.is_empty(), "symbol must not be empty");
            let mut matches = find_matches(text, &symbol, false);
            if matches.is_empty() {
                matches = find_matches(text, &symbol, true);
            }
            ensure!(
                !matches.is_empty(),
                "Symbol \"{symbol}\" was not found on line {line}"
            );
            ensure!(
                occurrence <= matches.len(),
                "Symbol \"{symbol}\" occurrence {occurrence} is out of bounds on line {line} (found {})",
                matches.len()
            );
            matches[occurrence - 1]
        }
    };
    let prefix = text.get(..byte).unwrap_or("");
    Ok(prefix.encode_utf16().count() as u32)
}

fn position_json(text: &str, line: u32, symbol: Option<&str>) -> Result<Value> {
    let line = line.max(1);
    let character = resolve_column(text, line, symbol)?;
    Ok(json!({"line": line - 1, "character": character}))
}

// =============================================================================
// 服务器结果格式化
// =============================================================================

fn parse_locations(value: &Value) -> Result<Vec<Location>> {
    let items: Vec<&Value> = match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![value],
        _ => bail!("the language server returned an unsupported navigation result"),
    };
    let mut locations = Vec::with_capacity(items.len());
    for item in items {
        let object = item
            .as_object()
            .context("the language server returned an unsupported navigation result")?;
        if object.contains_key("uri") {
            let location: Location = serde_json::from_value(item.clone())
                .context("the language server returned an invalid Location")?;
            locations.push(location);
        } else if object.contains_key("targetUri") {
            let link: LocationLink = serde_json::from_value(item.clone())
                .context("the language server returned an invalid LocationLink")?;
            let range = link
                .target_selection_range
                .or(link.target_range)
                .context("the language server returned a LocationLink without a target range")?;
            locations.push(Location {
                uri: link.target_uri,
                range,
            });
        } else {
            bail!("the language server returned a navigation entry without uri or targetUri");
        }
    }
    Ok(locations)
}

fn format_location(cwd: &Path, location: &Location) -> String {
    let path = uri::uri_to_path(&location.uri);
    let file = match &path {
        Some(path) => display_path(cwd, path),
        None => location.uri.clone(),
    };
    format!(
        "{file}:{}:{}",
        location.range.start.line.saturating_add(1),
        location.range.start.character.saturating_add(1)
    )
}

fn format_location_with_context(cwd: &Path, location: &Location) -> String {
    let header = format!("  {}", format_location(cwd, location));
    let Some(path) = uri::uri_to_path(&location.uri) else {
        return header;
    };
    let Ok(text) = client::read_document_text(&path) else {
        return header;
    };
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.is_empty() {
        return header;
    }
    let line = location.range.start.line.saturating_add(1);
    let total = lines.len() as u32;
    let start = line.saturating_sub(CONTEXT_LINES).max(1);
    let end = (line.saturating_add(CONTEXT_LINES)).min(total);
    let mut rendered = vec![header];
    for current in start..=end {
        let content = lines
            .get(current.saturating_sub(1) as usize)
            .copied()
            .unwrap_or("");
        let content = content.strip_suffix('\r').unwrap_or(content);
        rendered.push(format!("    {current}: {content}"));
    }
    rendered.join("\n")
}

fn is_only_queried_declaration(locations: &[Location], uri: &str, position: &Value) -> bool {
    let (Some(line), Some(character)) = (
        position.get("line").and_then(Value::as_u64),
        position.get("character").and_then(Value::as_u64),
    ) else {
        return false;
    };
    let (line, character) = (line as u32, character as u32);
    let contains = |range: &crate::lsp::types::Range| {
        let start = (range.start.line, range.start.character);
        let end = (range.end.line, range.end.character);
        start <= (line, character) && (line, character) <= end
    };
    matches!(locations, [only] if only.uri == uri && contains(&only.range))
}

fn extract_hover_text(contents: &Value) -> String {
    match contents {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(extract_hover_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(map) => map
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| serde_json::to_string(contents).unwrap_or_default()),
        other => other.to_string(),
    }
}

fn symbol_kind_name(kind: Option<u8>) -> &'static str {
    match kind {
        Some(1) => "File",
        Some(2) => "Module",
        Some(3) => "Namespace",
        Some(4) => "Package",
        Some(5) => "Class",
        Some(6) => "Method",
        Some(7) => "Property",
        Some(8) => "Field",
        Some(9) => "Constructor",
        Some(10) => "Enum",
        Some(11) => "Interface",
        Some(12) => "Function",
        Some(13) => "Variable",
        Some(14) => "Constant",
        Some(15) => "String",
        Some(16) => "Number",
        Some(17) => "Boolean",
        Some(18) => "Array",
        Some(19) => "Object",
        Some(20) => "Key",
        Some(21) => "Null",
        Some(22) => "EnumMember",
        Some(23) => "Struct",
        Some(24) => "Event",
        Some(25) => "Operator",
        Some(26) => "TypeParameter",
        _ => "Unknown",
    }
}

enum SymbolEntry {
    Document(DocumentSymbol),
    Flat(SymbolInformation),
}

fn parse_symbols(value: &Value) -> Result<Vec<SymbolEntry>> {
    let items: Vec<&Value> = match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().collect(),
        _ => bail!("the language server returned an invalid symbol list"),
    };
    let mut symbols = Vec::with_capacity(items.len());
    for item in items {
        let object = item
            .as_object()
            .context("the language server returned an invalid symbol")?;
        if object.contains_key("selectionRange") {
            let symbol: DocumentSymbol = serde_json::from_value(item.clone())
                .context("the language server returned an invalid document symbol")?;
            symbols.push(SymbolEntry::Document(symbol));
        } else {
            let symbol: SymbolInformation = serde_json::from_value(item.clone())
                .context("the language server returned an invalid symbol")?;
            symbols.push(SymbolEntry::Flat(symbol));
        }
    }
    Ok(symbols)
}

fn format_document_symbol(symbol: &DocumentSymbol, indent: usize) -> Vec<String> {
    let prefix = "  ".repeat(indent);
    let detail = symbol
        .detail
        .as_deref()
        .map(|detail| format!(" {detail}"))
        .unwrap_or_default();
    let line = symbol
        .range
        .as_ref()
        .map(|range| range.start.line.saturating_add(1).to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    let mut lines = vec![format!(
        "{prefix}{} {}{detail} @ line {line}",
        symbol_kind_name(symbol.kind),
        symbol.name
    )];
    for child in &symbol.children {
        lines.extend(format_document_symbol(child, indent + 1));
    }
    lines
}

fn format_flat_symbol(cwd: &Path, symbol: &SymbolInformation) -> String {
    let container = symbol
        .container_name
        .as_deref()
        .map(|name| format!(" ({name})"))
        .unwrap_or_default();
    format!(
        "{} {}{container} @ {}",
        symbol_kind_name(symbol.kind),
        symbol.name,
        format_location(cwd, &symbol.location)
    )
}

fn filter_workspace_symbols(
    symbols: Vec<SymbolInformation>,
    query: &str,
) -> Vec<SymbolInformation> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return symbols;
    }
    symbols
        .into_iter()
        .filter(|symbol| {
            symbol.name.to_lowercase().contains(&needle)
                || symbol
                    .container_name
                    .as_deref()
                    .is_some_and(|name| name.to_lowercase().contains(&needle))
                || symbol.location.uri.to_lowercase().contains(&needle)
        })
        .collect()
}

fn dedupe_workspace_symbols(symbols: Vec<SymbolInformation>) -> Vec<SymbolInformation> {
    let mut seen = BTreeSet::new();
    let mut unique = Vec::with_capacity(symbols.len());
    for symbol in symbols {
        let key = (
            symbol.name.clone(),
            symbol.container_name.clone().unwrap_or_default(),
            symbol.kind.unwrap_or(0),
            symbol.location.uri.clone(),
            symbol.location.range.start.line,
            symbol.location.range.start.character,
        );
        if seen.insert(key) {
            unique.push(symbol);
        }
    }
    unique
}

// =============================================================================
// status / capabilities / reload
// =============================================================================

fn status(registry: &Registry) -> Result<Value> {
    let action = Action::Status;
    if let Some(error) = registry.config_error() {
        return Ok(done(
            action,
            false,
            format!(
                "LSP configuration is invalid, so no language server can start until it is fixed:\n{error}"
            ),
        ));
    }
    let discovered = registry.discovered();
    let mut lines = vec![format!(
        "Language servers for {}:",
        registry.cwd().display()
    )];
    if discovered.is_empty() {
        let known = registry.definitions().len();
        lines.push("  none available".to_owned());
        if known > 0 {
            lines.push(format!(
                "  {known} built-in definition(s) are known, but none match this workspace or their executables are not on PATH."
            ));
        }
    } else {
        for definition in &discovered {
            let label = match registry.state(&definition.name) {
                crate::lsp::ClientState::NotStarted => "not started".to_owned(),
                crate::lsp::ClientState::Initializing => "initializing".to_owned(),
                crate::lsp::ClientState::Ready => "ready".to_owned(),
                crate::lsp::ClientState::Failed(reason) => format!("failed: {reason}"),
            };
            lines.push(format!("  {}: {label}", definition.name));
        }
    }
    let configured: Vec<String> = discovered.iter().map(|d| d.name.clone()).collect();
    for client in registry.started() {
        if !configured.iter().any(|name| name == &client.name) {
            lines.push(format!(
                "  {}: ready (started under a previous configuration)",
                client.name
            ));
        }
    }
    if !discovered.is_empty() {
        lines.push(
            "Note: 'not started' means the server was discovered but no request has started it yet; \
             'ready' means a live process exists for this workspace."
                .to_owned(),
        );
    }
    let server = configured.join(", ");
    Ok(with_server(done(action, true, lines.join("\n")), &server))
}

fn capabilities(
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let action = Action::Capabilities;
    let definitions = match params.file.as_deref().filter(|file| *file != "*") {
        Some(file) => {
            let path = resolve_path(registry.cwd(), file);
            let servers = real_servers_for(registry, &path);
            if servers.is_empty() {
                return Ok(done(
                    action,
                    false,
                    format!(
                        "No language server is configured for {}.",
                        display_path(registry.cwd(), &path)
                    ),
                ));
            }
            servers
        }
        None => all_real_servers(registry),
    };
    if definitions.is_empty() {
        return Ok(done(
            action,
            false,
            "No language server is available for this workspace.".to_owned(),
        ));
    }
    let mut sections = Vec::new();
    let mut responded = Vec::new();
    for definition in &definitions {
        check(cancellation)?;
        match registry.ensure_client(&definition.name, deadline, cancellation) {
            Ok(client) => {
                responded.push(definition.name.clone());
                let capabilities = client.capabilities_json();
                let pretty = serde_json::to_string_pretty(&capabilities)
                    .unwrap_or_else(|_| capabilities.to_string());
                sections.push(format!(
                    "{}:\n  capabilities: {}",
                    definition.name,
                    pretty.replace('\n', "\n  ")
                ));
            }
            Err(error) if is_control(&error) => return Err(error),
            Err(error) => sections.push(format!(
                "{}: failed to start ({})",
                definition.name,
                crate::logging::diagnostic_text(&format!("{error:#}"))
            )),
        }
    }
    let success = !responded.is_empty();
    Ok(with_server(
        done(action, success, sections.join("\n")),
        &responded.join(", "),
    ))
}

fn reload(
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    registry.reload_config()?;
    let definitions = match params.file.as_deref().filter(|file| *file != "*") {
        Some(file) => real_servers_for(registry, &resolve_path(registry.cwd(), file)),
        None => all_real_servers(registry),
    };
    let mut notes = Vec::new();
    let mut success = !definitions.is_empty();
    let mut evicted = BTreeSet::new();
    for definition in &definitions {
        if let Some(client) = registry.client(&definition.name)
            && evicted.insert(client.identity.clone())
        {
            registry.evict(&definition.name, &client)?;
        }
    }
    for definition in definitions {
        registry.clear_failure(&definition.name);
        match registry.ensure_client(&definition.name, deadline, cancellation) {
            Ok(_) => notes.push(format!("Reconnected {}", definition.name)),
            Err(error) if is_control(&error) => return Err(error),
            Err(error) => {
                success = false;
                notes.push(crate::logging::diagnostic_text(&format!(
                    "{}: {error:#}",
                    definition.name
                )));
            }
        }
    }
    Ok(done(Action::Reload, success, notes.join("\n")))
}

// =============================================================================
// diagnostics
// =============================================================================

fn diagnostics_action(
    tools: &Tools,
    registry: &Registry,
    params: &Params,
    _access: Access,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let action = Action::Diagnostics;
    let file = params.file()?;
    ensure!(
        file != "*",
        "Workspace diagnostics are not run by LSP. Use bash for cargo check, tsc, go build or other project checks."
    );
    // 未显式给 timeout 时诊断等待沿用 requestTimeout；显式 timeout 已经缩短
    // 了 call deadline，两者共用同一个绝对期限。
    let diagnostic_deadline = match params.timeout {
        Some(_) => deadline,
        None => deadline.min(Instant::now() + registry.settings().request_budget()),
    };
    let targets = diagnostics::resolve_targets(registry.cwd(), file, cancellation)?;
    if targets.paths.is_empty() {
        return Ok(done(
            action,
            false,
            format!("No files matched pattern: {file}"),
        ));
    }
    let mut sections = Vec::new();
    if targets.truncated {
        sections.push(format!(
            "Pattern matched more than {} files; showing the first {} in sorted order.",
            diagnostics::MAX_TARGETS,
            diagnostics::MAX_TARGETS
        ));
    }
    let mut success = true;
    let mut reports = Vec::with_capacity(targets.paths.len());
    for path in &targets.paths {
        check(cancellation)?;
        let report = diagnostics::collect(
            registry,
            path,
            "lsp",
            owner(tools),
            false,
            diagnostic_deadline,
            cancellation,
        )?;
        // 全部服务器失败才不成功；未验证或没有能力都显式出现在正文里。
        success &= report
            .servers
            .iter()
            .any(|server| server.status != ReportStatus::Failed);
        sections.push(diagnostics::render(&report, None));
        reports.push(report);
    }
    let content = sections.join("\n\n");
    let value = done(action, success, content);
    let structured = if reports.len() == 1 {
        json!(reports.pop())
    } else {
        Value::Array(reports.into_iter().map(|report| json!(report)).collect())
    };
    Ok(with_diagnostics(value, structured))
}

// =============================================================================
// 导航 / hover
// =============================================================================

fn navigation(
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let action = params.action;
    let file = params.file()?;
    ensure!(file != "*", "{} requires a concrete file", action.as_str());
    let path = resolve_path(registry.cwd(), file);
    let definitions = real_servers_for(registry, &path);
    let Some(definition) = definitions.first().cloned() else {
        return Ok(done(
            action,
            false,
            format!(
                "No language server covers {}.",
                display_path(registry.cwd(), &path)
            ),
        ));
    };
    let client = registry.ensure_client(&definition.name, deadline, cancellation)?;
    let aware = project_aware(&definition);
    let supported = match action {
        Action::Definition => client.capabilities.definition,
        Action::TypeDefinition => client.capabilities.type_definition,
        Action::Implementation => client.capabilities.implementation,
        Action::References => client.capabilities.references,
        Action::Hover => client.capabilities.hover,
        _ => false,
    };
    if !supported {
        return Ok(with_server(
            done(
                action,
                false,
                format!(
                    "{} does not support {} in this workspace.",
                    definition.name,
                    method_for(action)
                ),
            ),
            &definition.name,
        ));
    }
    if let Some(line) = params.line
        && params.symbol.as_deref().is_none_or(str::is_empty)
        && aware
        && SYMBOL_REQUIRED_ACTIONS.contains(&action.as_str())
    {
        bail!(
            "symbol is required for project-aware {} on line {line}; pass symbol=<name>, optionally name#N for the Nth occurrence",
            action.as_str()
        );
    }
    wait_for_project(&client, &definition, action, &path, deadline, cancellation)?;
    let line = params.line.unwrap_or(1);
    let mut service = deny_service();
    let (content, useless) =
        client.operation(deadline, cancellation, &mut service, |op| {
            let state = client.open_or_refresh(op, &path)?;
            let position = position_json(&state.text, line, params.symbol.as_deref())?;
            let uri = state.uri.clone();
            match action {
                Action::Definition | Action::TypeDefinition | Action::Implementation => {
                    let method = match action {
                        Action::Definition => "textDocument/definition",
                        Action::TypeDefinition => "textDocument/typeDefinition",
                        _ => "textDocument/implementation",
                    };
                    let value = op.request(
                        method,
                        json!({"textDocument": {"uri": uri}, "position": position}),
                    )?;
                    let locations = parse_locations(&value)?;
                    if locations.is_empty() {
                        return Ok((empty_navigation(action), true));
                    }
                    let lines: Vec<String> = locations
                        .iter()
                        .map(|location| format_location_with_context(registry.cwd(), location))
                        .collect();
                    Ok((
                        format!(
                            "Found {} {}:\n{}",
                            locations.len(),
                            noun_for(action),
                            lines.join("\n")
                        ),
                        false,
                    ))
                }
                Action::References => {
                    let mut locations = Vec::new();
                    for attempt in 0..=REFERENCES_RETRY_COUNT {
                        let value = op.request(
                            "textDocument/references",
                            json!({
                                "textDocument": {"uri": uri},
                                "position": position,
                                "context": {"includeDeclaration": true},
                            }),
                        )?;
                        locations = parse_locations(&value)?;
                        if !aware || attempt == REFERENCES_RETRY_COUNT {
                            break;
                        }
                        if !locations.is_empty()
                            && !is_only_queried_declaration(&locations, &uri, &position)
                        {
                            break;
                        }
                        check(cancellation)?;
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        std::thread::sleep(REFERENCES_RETRY_DELAY.min(deadline - now));
                    }
                    if locations.is_empty() {
                        return Ok(("No references found".to_owned(), true));
                    }
                    let mut lines: Vec<String> = locations
                        .iter()
                        .take(REFERENCE_CONTEXT_LIMIT)
                        .map(|location| format_location_with_context(registry.cwd(), location))
                        .collect();
                    if locations.len() > REFERENCE_CONTEXT_LIMIT {
                        lines.push(format!(
                            "  ... {} additional reference(s) shown without context",
                            locations.len() - REFERENCE_CONTEXT_LIMIT
                        ));
                        lines.extend(locations.iter().skip(REFERENCE_CONTEXT_LIMIT).map(
                            |location| format!("  {}", format_location(registry.cwd(), location)),
                        ));
                    }
                    Ok((
                        format!(
                            "Found {} reference(s):\n{}",
                            locations.len(),
                            lines.join("\n")
                        ),
                        false,
                    ))
                }
                Action::Hover => {
                    let value = op.request(
                        "textDocument/hover",
                        json!({"textDocument": {"uri": uri}, "position": position}),
                    )?;
                    let contents = value.get("contents").cloned().unwrap_or(Value::Null);
                    let text = extract_hover_text(&contents);
                    if text.trim().is_empty() {
                        return Ok(("No hover information".to_owned(), true));
                    }
                    Ok((text, false))
                }
                _ => unreachable!("navigation dispatches only navigation actions"),
            }
        })?;
    let value = with_server(done(action, true, content), &definition.name);
    Ok(if useless { with_useless(value) } else { value })
}

fn method_for(action: Action) -> &'static str {
    match action {
        Action::Definition => "definitions",
        Action::TypeDefinition => "type definitions",
        Action::Implementation => "implementations",
        Action::References => "references",
        Action::Hover => "hover information",
        _ => "this request",
    }
}

fn noun_for(action: Action) -> &'static str {
    match action {
        Action::Definition => "definition(s)",
        Action::TypeDefinition => "type definition(s)",
        Action::Implementation => "implementation(s)",
        _ => "result(s)",
    }
}

fn empty_navigation(action: Action) -> String {
    match action {
        Action::Definition => "No definition found".to_owned(),
        Action::TypeDefinition => "No type definition found".to_owned(),
        Action::Implementation => "No implementation found".to_owned(),
        _ => "No results found".to_owned(),
    }
}

// =============================================================================
// symbols
// =============================================================================

fn symbols(
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let action = Action::Symbols;
    let file = params.file()?;
    if file == "*" {
        return workspace_symbols(registry, params, deadline, cancellation);
    }
    let path = resolve_path(registry.cwd(), file);
    let definitions = real_servers_for(registry, &path);
    let Some(definition) = definitions.first().cloned() else {
        return Ok(done(
            action,
            false,
            format!(
                "No language server covers {}.",
                display_path(registry.cwd(), &path)
            ),
        ));
    };
    let client = registry.ensure_client(&definition.name, deadline, cancellation)?;
    if !client.capabilities.document_symbol {
        return Ok(with_server(
            done(
                action,
                false,
                format!("{} does not support document symbols.", definition.name),
            ),
            &definition.name,
        ));
    }
    let mut service = deny_service();
    let symbols = client.operation(deadline, cancellation, &mut service, |op| {
        let state = client.open_or_refresh(op, &path)?;
        let value = op.request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": state.uri}}),
        )?;
        parse_symbols(&value)
    })?;
    if symbols.is_empty() {
        return Ok(with_useless(with_server(
            done(action, true, "No symbols found".to_owned()),
            &definition.name,
        )));
    }
    let relative = display_path(registry.cwd(), &path);
    let mut lines = vec![format!("Symbols in {relative}:")];
    for symbol in &symbols {
        match symbol {
            SymbolEntry::Document(symbol) => lines.extend(format_document_symbol(symbol, 0)),
            SymbolEntry::Flat(symbol) => {
                lines.push(format!("  {}", format_flat_symbol(registry.cwd(), symbol)));
            }
        }
    }
    Ok(with_server(
        done(action, true, lines.join("\n")),
        &definition.name,
    ))
}

fn workspace_symbols(
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let action = Action::Symbols;
    let query = params
        .query
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .context("query is required for workspace symbol search (file=\"*\")")?
        .to_owned();
    let definitions = all_real_servers(registry);
    if definitions.is_empty() {
        return Ok(done(
            action,
            false,
            "No language server is available for workspace symbol search.".to_owned(),
        ));
    }
    let mut aggregated = Vec::new();
    let mut responded = Vec::new();
    let mut failures = Vec::new();
    for definition in &definitions {
        check(cancellation)?;
        let client = match registry.ensure_client(&definition.name, deadline, cancellation) {
            Ok(client) => client,
            Err(error) if is_control(&error) => return Err(error),
            Err(error) => {
                failures.push(format!(
                    "  {}: {}",
                    definition.name,
                    crate::logging::diagnostic_text(&format!("{error:#}"))
                ));
                continue;
            }
        };
        let mut service = deny_service();
        let outcome = client.operation(deadline, cancellation, &mut service, |op| {
            Ok(op.request_raw("workspace/symbol", json!({"query": query})))
        });
        let value = match outcome {
            Ok(Ok(value)) => {
                responded.push(definition.name.clone());
                value
            }
            Ok(Err(Failure::Cancelled)) => return Err(anyhow::Error::new(Cancelled)),
            Ok(Err(Failure::Rpc(-32601))) => continue,
            Ok(Err(failure)) => {
                failures.push(format!(
                    "  {}: {}",
                    definition.name,
                    failure_error(failure, "workspace/symbol")
                ));
                continue;
            }
            Err(error) if is_control(&error) => return Err(error),
            Err(error) => {
                failures.push(format!(
                    "  {}: {}",
                    definition.name,
                    crate::logging::diagnostic_text(&format!("{error:#}"))
                ));
                continue;
            }
        };
        let symbols: Vec<SymbolInformation> = match &value {
            Value::Null => Vec::new(),
            Value::Array(_) => serde_json::from_value(value.clone())
                .context("the language server returned an invalid workspace symbol list")?,
            _ => bail!("the language server returned an invalid workspace symbol list"),
        };
        aggregated.extend(filter_workspace_symbols(symbols, &query));
    }
    let failure_section = if failures.is_empty() {
        String::new()
    } else {
        format!("\nServer failures:\n{}", failures.join("\n"))
    };
    if responded.is_empty() {
        return Ok(with_server(
            done(
                action,
                false,
                format!(
                    "Workspace symbol search failed: all language servers failed{failure_section}"
                ),
            ),
            "",
        ));
    }
    let deduped = dedupe_workspace_symbols(aggregated);
    if deduped.is_empty() {
        return Ok(with_server(
            done(
                action,
                true,
                format!("No symbols matching \"{query}\"{failure_section}"),
            ),
            &responded.join(", "),
        ));
    }
    let limited: Vec<&SymbolInformation> = deduped.iter().take(WORKSPACE_SYMBOL_LIMIT).collect();
    let mut lines: Vec<String> = limited
        .iter()
        .map(|symbol| format!("  {}", format_flat_symbol(registry.cwd(), symbol)))
        .collect();
    if deduped.len() > WORKSPACE_SYMBOL_LIMIT {
        lines.push(format!(
            "\n[…{} symbol(s) elided…]",
            deduped.len() - WORKSPACE_SYMBOL_LIMIT
        ));
    }
    Ok(with_server(
        done(
            action,
            true,
            format!(
                "Found {} symbol(s) matching \"{query}\":\n{}{failure_section}",
                deduped.len(),
                lines.join("\n")
            ),
        ),
        &responded.join(", "),
    ))
}

// =============================================================================
// 编辑结果与 applyEdit 回调
// =============================================================================

fn edit_is_empty(edit: &WorkspaceEdit) -> bool {
    let no_changes = edit
        .changes
        .as_ref()
        .is_none_or(|changes| changes.values().all(Vec::is_empty));
    let no_document_changes = edit
        .document_changes
        .as_ref()
        .is_none_or(|changes| changes.is_empty());
    no_changes && no_document_changes
}

fn parse_workspace_edit(value: &Value) -> Result<WorkspaceEdit> {
    if value.is_null() {
        return Ok(WorkspaceEdit::default());
    }
    serde_json::from_value(value.clone())
        .context("the language server returned an invalid workspace edit")
}

// =============================================================================
// rename
// =============================================================================

/// 在已持有的 operation 里发起 `textDocument/rename`，不重新进入 operation。
fn rename_request(
    op: &mut crate::lsp::client::Op<'_>,
    client: &Client,
    path: &Path,
    line: u32,
    symbol: Option<&str>,
    new_name: &str,
) -> Result<WorkspaceEdit> {
    let state = client.open_or_refresh(op, path)?;
    let position = position_json(&state.text, line, symbol)?;
    let value = op.request(
        "textDocument/rename",
        json!({
            "textDocument": {"uri": state.uri},
            "position": position,
            "newName": new_name,
        }),
    )?;
    parse_workspace_edit(&value)
}

fn rename(
    tools: &Tools,
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let action = Action::Rename;
    let file = params.file()?;
    ensure!(file != "*", "rename requires a concrete file");
    let new_name = params
        .new_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .context("rename requires `new_name`")?
        .to_owned();
    let path = resolve_path(registry.cwd(), file);
    let definitions = real_servers_for(registry, &path);
    let Some(definition) = definitions.first().cloned() else {
        return Ok(done(
            action,
            false,
            format!(
                "No language server covers {}.",
                display_path(registry.cwd(), &path)
            ),
        ));
    };
    let client = registry.ensure_client(&definition.name, deadline, cancellation)?;
    if !client.capabilities.rename {
        return Ok(with_server(
            done(
                action,
                false,
                format!("{} does not support rename.", definition.name),
            ),
            &definition.name,
        ));
    }
    let aware = project_aware(&definition);
    if let Some(line) = params.line
        && params.symbol.as_deref().is_none_or(str::is_empty)
        && aware
    {
        bail!(
            "symbol is required for project-aware rename on line {line}; pass symbol=<name>, optionally name#N"
        );
    }
    wait_for_project(&client, &definition, action, &path, deadline, cancellation)?;
    let mut service = deny_service();
    let edit = client.operation(deadline, cancellation, &mut service, |op| {
        rename_request(
            op,
            &client,
            &path,
            params.line.unwrap_or(1),
            params.symbol.as_deref(),
            &new_name,
        )
    })?;
    suggestion_result(tools, &client, action, &edit, cancellation)
}

// =============================================================================
// rename_file
// =============================================================================

struct RenamePair {
    old: PathBuf,
    new: PathBuf,
}

struct RenameEnumeration {
    pairs: Vec<RenamePair>,
    exceeded: bool,
}

fn enumerate_rename_pairs(
    source: &Path,
    dest: &Path,
    directory: bool,
) -> Result<RenameEnumeration> {
    if !directory {
        return Ok(RenameEnumeration {
            pairs: vec![RenamePair {
                old: source.to_path_buf(),
                new: dest.to_path_buf(),
            }],
            exceeded: false,
        });
    }
    let mut pairs = Vec::new();
    let mut stack = vec![source.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current)
            .with_context(|| format!("reading directory {}", current.display()))?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                bail!(
                    "{} is a symlink; a directory rename only covers regular files",
                    path.display()
                );
            }
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            ensure!(
                metadata.is_file(),
                "{} is not a regular file; a directory rename only covers regular files",
                path.display()
            );
            let Ok(relative) = path.strip_prefix(source) else {
                continue;
            };
            if pairs.len() >= MAX_RENAME_PAIRS {
                return Ok(RenameEnumeration {
                    pairs,
                    exceeded: true,
                });
            }
            pairs.push(RenamePair {
                old: path.clone(),
                new: dest.join(relative),
            });
        }
    }
    pairs.sort_by(|left, right| left.old.cmp(&right.old));
    Ok(RenameEnumeration {
        pairs,
        exceeded: false,
    })
}

fn rename_file(
    tools: &Tools,
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let source = resolve_path(registry.cwd(), params.file()?);
    let dest = resolve_path(
        registry.cwd(),
        params
            .new_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .context("rename_file requires new_name")?,
    );
    tools.workspace().components(&source)?;
    tools.workspace().components(&dest)?;
    ensure!(
        source != dest && !dest.starts_with(&source),
        "Invalid rename destination"
    );
    let meta = std::fs::symlink_metadata(&source)?;
    ensure!(
        !meta.file_type().is_symlink(),
        "Rename source must not be a symlink"
    );
    ensure!(
        meta.is_file() || meta.is_dir(),
        "Rename source must be a regular file or directory"
    );
    edits::validate_move(tools, &source, &dest, cancellation)?;
    let enumeration = enumerate_rename_pairs(&source, &dest, meta.is_dir())?;
    ensure!(
        !enumeration.exceeded,
        "Directory exceeds {MAX_RENAME_PAIRS} files; query smaller batches"
    );
    let pairs: Vec<Value> = enumeration
        .pairs
        .iter()
        .map(|pair| {
            json!({
        "oldUri":uri::path_to_uri(&pair.old),"newUri":uri::path_to_uri(&pair.new)})
        })
        .collect();
    let definitions = all_real_servers(registry);
    let mut suggestions = Vec::new();
    let mut failures = Vec::new();
    for definition in definitions {
        if !enumeration
            .pairs
            .iter()
            .any(|pair| definition_matches(registry, &definition.name, &pair.old))
        {
            continue;
        }
        let server_pairs: Vec<Value> = enumeration
            .pairs
            .iter()
            .zip(&pairs)
            .filter(|(pair, _)| definition_matches(registry, &definition.name, &pair.old))
            .map(|(_, value)| value.clone())
            .collect();
        let outcome = (|| -> Result<Value> {
            let client = registry.ensure_client(&definition.name, deadline, cancellation)?;
            ensure!(
                client
                    .capabilities
                    .raw
                    .pointer("/workspace/fileOperations/willRename")
                    .is_some_and(|v| v.is_object() || v == &Value::Bool(true)),
                "Server does not support willRenameFiles"
            );
            let mut deny = deny_service();
            let edit = client.operation(deadline, cancellation, &mut deny, |op| {
                for pair in &enumeration.pairs {
                    if definition_matches(registry, &definition.name, &pair.old) {
                        client.open_or_refresh(op, &pair.old)?;
                    }
                }
                parse_workspace_edit(
                    &op.request("workspace/willRenameFiles", json!({"files":server_pairs}))?,
                )
            })?;
            suggestion_result(tools, &client, Action::RenameFile, &edit, cancellation)
        })();
        match outcome {
            Ok(value) => suggestions.push(value),
            Err(error) if is_control(&error) => return Err(error),
            Err(error) => failures.push(json!({"server":definition.name,"error":crate::logging::diagnostic_text(&format!("{error:#}"))})),
        }
    }
    let conflicts = suggestion_conflicts(&suggestions);
    let complete = failures.is_empty() && !suggestions.is_empty() && conflicts.is_empty();
    let mut value = done(
        Action::RenameFile,
        complete,
        format!(
            "Rename suggestion only: {} -> {}. No files were moved. Server suggestions are independent; compare overlapping changes before editing.\n{}",
            source.display(),
            dest.display(),
            serde_json::to_string_pretty(&suggestions)?
        ),
    );
    value["details"]["applied"] = json!(false);
    value["details"]["complete"] = json!(complete);
    value["details"]["suggestions"] = json!(suggestions);
    value["details"]["failures"] = json!(failures);
    value["details"]["conflicts"] = json!(conflicts);
    value["details"]["move"] = json!({"kind":"rename","oldUri":uri::path_to_uri(&source),"newUri":uri::path_to_uri(&dest)});
    Ok(value)
}

fn definition_matches(registry: &Registry, name: &str, path: &Path) -> bool {
    real_servers_for(registry, path)
        .iter()
        .any(|definition| definition.name == name)
}

fn suggestion_result(
    tools: &Tools,
    client: &Client,
    action: Action,
    edit: &WorkspaceEdit,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let suggestion = edits::suggest(tools, client, edit, cancellation)?;
    let mut result = with_server(
        done(
            action,
            true,
            format!(
                "Modification suggestions only; no files were changed. Read files again before using edit/write/bash.\n{}",
                serde_json::to_string_pretty(&suggestion)?
            ),
        ),
        &client.name,
    );
    result["details"]["applied"] = json!(false);
    result["details"]["suggestion"] = suggestion;
    result["details"]["complete"] = json!(true);
    if edit_is_empty(edit) {
        result = with_useless(result);
    }
    Ok(result)
}

fn format_suggestions(
    tools: &Tools,
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let path = resolve_path(registry.cwd(), params.file()?);
    ensure!(params.file()? != "*", "format requires a concrete file");
    let mut results = Vec::new();
    let mut failures = Vec::new();
    for definition in real_servers_for(registry, &path) {
        let outcome = (|| -> Result<Value> {
            let client = registry.ensure_client(&definition.name, deadline, cancellation)?;
            ensure!(
                client.capabilities.document_formatting,
                "Formatting is unsupported"
            );
            let mut deny = deny_service();
            let edit = client.operation(deadline,cancellation,&mut deny,|op| {
                let state = client.open_or_refresh(op,&path)?;
                let value = op.request("textDocument/formatting",json!({"textDocument":{"uri":state.uri},"options":crate::lsp::format_options::resolve(&path,&state.text)}))?;
                let edits: Vec<TextEdit> = if value.is_null() {Vec::new()} else {serde_json::from_value(value).context("Invalid formatting edits")?};
                Ok(WorkspaceEdit{changes:None,document_changes:Some(vec![json!({"textDocument":{"uri":state.uri,"version":state.version},"edits":edits})])})
            })?;
            suggestion_result(tools, &client, Action::Format, &edit, cancellation)
        })();
        match outcome {
            Ok(value)=>results.push(value),
            Err(error) if is_control(&error)=>return Err(error),
            Err(error)=>failures.push(json!({"server":definition.name,"error":crate::logging::diagnostic_text(&format!("{error:#}"))})),
        }
    }
    let conflicts = suggestion_conflicts(&results);
    let mut result = done(
        Action::Format,
        !results.is_empty() && failures.is_empty(),
        format!(
            "Formatting suggestions only; no files were changed.\n{}",
            serde_json::to_string_pretty(&results)?
        ),
    );
    result["details"]["complete"] =
        json!(!results.is_empty() && failures.is_empty() && conflicts.is_empty());
    result["details"]["conflicts"] = json!(conflicts);
    result["details"]["applied"] = json!(false);
    result["details"]["suggestions"] = json!(results);
    result["details"]["failures"] = json!(failures);
    Ok(result)
}

/// 多服务器候选按文件比较，仅报告冲突，不选优先级或合并编辑。
fn suggestion_conflicts(suggestions: &[Value]) -> Vec<Value> {
    let mut seen: BTreeMap<&str, (&Value, &Value)> = BTreeMap::new();
    let mut conflicts = Vec::new();
    for suggestion in suggestions {
        let details = &suggestion["details"];
        let server = &details["serverName"];
        if let Some(files) = details["suggestion"]["files"].as_array() {
            for file in files {
                if let Some(path) = file["path"].as_str() {
                    if let Some((prior, owner)) = seen.get(path) {
                        if prior["before"] != file["before"] || prior["after"] != file["after"] {
                            conflicts.push(json!({"path":path,"servers":[owner,server]}));
                        }
                    } else {
                        seen.insert(path, (file, server));
                    }
                }
            }
        }
    }
    conflicts
}

// =============================================================================
// code_actions
// =============================================================================

enum CodeActionEntry {
    Command(Command),
    Action(ActionItem),
}

struct ActionItem {
    raw: Value,
    title: String,
    kind: Option<String>,
    preferred: bool,
    disabled: Option<String>,
    edit: Option<WorkspaceEdit>,
    command: Option<Command>,
}

impl CodeActionEntry {
    fn title(&self) -> &str {
        match self {
            Self::Command(command) => &command.title,
            Self::Action(item) => &item.title,
        }
    }
}

fn parse_action_entry(value: &Value) -> Result<CodeActionEntry> {
    let object = value
        .as_object()
        .context("the language server returned an invalid code action")?;
    if object.get("command").is_some_and(Value::is_string) {
        let command: Command = serde_json::from_value(value.clone())
            .context("the language server returned an invalid command")?;
        return Ok(CodeActionEntry::Command(command));
    }
    let action: crate::lsp::types::CodeAction = serde_json::from_value(value.clone())
        .context("the language server returned an invalid code action")?;
    let disabled = action.disabled.as_ref().map(|disabled| {
        disabled
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| "disabled by the language server".to_owned())
    });
    Ok(CodeActionEntry::Action(ActionItem {
        raw: value.clone(),
        title: action.title,
        kind: action.kind,
        preferred: object
            .get("isPreferred")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        disabled,
        edit: action.edit,
        command: action.command,
    }))
}

fn parse_code_actions(value: &Value) -> Result<Vec<CodeActionEntry>> {
    let items: Vec<&Value> = match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().collect(),
        _ => bail!("the language server returned an invalid code action list"),
    };
    items.into_iter().map(parse_action_entry).collect()
}

fn format_code_action(index: usize, entry: &CodeActionEntry) -> String {
    match entry {
        CodeActionEntry::Command(command) => {
            format!("{index}: [unsupported server command] {}", command.title)
        }
        CodeActionEntry::Action(item) => {
            let kind = item.kind.as_deref().unwrap_or("action");
            let preferred = if item.preferred { " (preferred)" } else { "" };
            let disabled = item
                .disabled
                .as_deref()
                .map(|reason| format!(" (disabled: {reason})"))
                .unwrap_or_default();
            format!("{index}: [{kind}] {}{preferred}{disabled}", item.title)
        }
    }
}

fn diagnostic_json(diagnostic: &Diagnostic) -> Value {
    let mut value = json!({
        "range": {
            "start": {
                "line": diagnostic.range.start.line,
                "character": diagnostic.range.start.character,
            },
            "end": {
                "line": diagnostic.range.end.line,
                "character": diagnostic.range.end.character,
            },
        },
        "message": diagnostic.message,
    });
    if let Some(severity) = diagnostic.severity {
        value["severity"] = json!(severity.0);
    }
    if let Some(code) = &diagnostic.code {
        value["code"] = code.clone();
    }
    if let Some(source) = &diagnostic.source {
        value["source"] = json!(source);
    }
    if !diagnostic.related_information.is_empty() {
        value["relatedInformation"] = json!(diagnostic.related_information);
    }
    if !diagnostic.tags.is_empty() {
        value["tags"] = json!(diagnostic.tags);
    }
    value
}

/// `code_actions` 的快速修复上下文：优先使用当前版本的缓存报告，否则做一次
/// 有界采集；诊断失败本身不会阻止重构类动作。
fn code_action_diagnostics(
    client: &Client,
    path: &Path,
    uri: &str,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Vec<Diagnostic>> {
    let expected = client.document(path).map(|state| state.version as u64);
    if let Some(entry) = client.diagnostics.snapshot(uri)
        && entry.seq > 0
        && entry
            .version
            .is_none_or(|version| Some(version) == expected)
    {
        return Ok(entry.diagnostics);
    }
    let budget = deadline.min(Instant::now() + CODE_ACTION_DIAGNOSTICS_WAIT);
    match client.collect_diagnostics(path, budget, cancellation) {
        Ok(outcome) => Ok(outcome.diagnostics.unwrap_or_default()),
        Err(error) if is_control(&error) => Err(error),
        Err(_) => Ok(Vec::new()),
    }
}

fn code_actions(
    tools: &Tools,
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let action = Action::CodeActions;
    let file = params.file()?;
    ensure!(file != "*", "code_actions requires a concrete file");
    let path = resolve_path(registry.cwd(), file);
    let definitions = real_servers_for(registry, &path);
    let Some(definition) = definitions.first().cloned() else {
        return Ok(done(
            action,
            false,
            format!(
                "No language server covers {}.",
                display_path(registry.cwd(), &path)
            ),
        ));
    };
    let client = registry.ensure_client(&definition.name, deadline, cancellation)?;
    if !client.capabilities.code_action {
        return Ok(with_server(
            done(
                action,
                false,
                format!("{} does not support code actions.", definition.name),
            ),
            &definition.name,
        ));
    }
    let query = params
        .query
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_owned);
    let line = params.line.unwrap_or(1);
    let cached_uri = client.document(&path).map(|state| state.uri);
    let diagnostics = match cached_uri {
        Some(uri) => code_action_diagnostics(&client, &path, &uri, deadline, cancellation)?,
        None => Vec::new(),
    };
    let diagnostics_json: Vec<Value> = diagnostics.iter().map(diagnostic_json).collect();
    let mut service = deny_service();
    let entries = client.operation(deadline, cancellation, &mut service, |op| {
        let state = client.open_or_refresh(op, &path)?;
        let position = position_json(&state.text, line, params.symbol.as_deref())?;
        let mut context = json!({
            "diagnostics": diagnostics_json,
            "triggerKind": 1,
        });
        if let Some(query) = &query {
            context["only"] = json!([query]);
        }
        let value = op.request(
            "textDocument/codeAction",
            json!({
                "textDocument": {"uri": state.uri},
                "range": {"start": position, "end": position},
                "context": context,
            }),
        )?;
        parse_code_actions(&value)
    })?;
    let selection = params.selection.as_deref();
    let list = selection.is_none();
    if list {
        if entries.is_empty() {
            return Ok(with_useless(with_server(
                done(action, true, "No code actions available".to_owned()),
                &definition.name,
            )));
        }
        let mut lines = vec![format!("{} code action(s):", entries.len())];
        lines.extend(
            entries
                .iter()
                .enumerate()
                .map(|(index, entry)| format!("  {}", format_code_action(index, entry))),
        );
        return Ok(with_server(
            done(action, true, lines.join("\n")),
            &definition.name,
        ));
    }
    let query = selection.expect("selection was checked above");
    let index = query.parse::<usize>().ok();
    let selected = match index {
        Some(index) => entries.get(index),
        None => {
            let needle = query.to_lowercase();
            entries
                .iter()
                .find(|entry| entry.title().to_lowercase().contains(&needle))
        }
    };
    let Some(selected) = selected else {
        let mut lines = vec![format!(
            "No code action matches \"{query}\". Available actions:"
        )];
        lines.extend(
            entries
                .iter()
                .enumerate()
                .map(|(index, entry)| format!("  {}", format_code_action(index, entry))),
        );
        return Ok(with_server(
            done(action, false, lines.join("\n")),
            &definition.name,
        ));
    };
    let selected_title = selected.title().to_owned();
    let mut item = match selected {
        CodeActionEntry::Command(command) => {
            return Ok(command_only(action, &definition.name, command));
        }
        CodeActionEntry::Action(item) => ActionItem {
            raw: item.raw.clone(),
            title: item.title.clone(),
            kind: item.kind.clone(),
            preferred: item.preferred,
            disabled: item.disabled.clone(),
            edit: item.edit.clone(),
            command: item.command.clone(),
        },
    };
    if let Some(reason) = &item.disabled {
        return Ok(with_server(
            done(
                action,
                false,
                format!("Action \"{selected_title}\" is disabled: {reason}"),
            ),
            &definition.name,
        ));
    }
    if item.edit.is_none()
        && client.capabilities.code_action_resolve
        && (item.command.is_none() || item.raw.get("data").is_some())
    {
        // resolve 只补全编辑；失败时不能继续应用一个不完整的动作。
        let mut service = deny_service();
        let resolved = client.operation(deadline, cancellation, &mut service, |op| {
            op.request("codeAction/resolve", item.raw.clone())
        })?;
        item = match parse_action_entry(&resolved)? {
            CodeActionEntry::Action(resolved) => resolved,
            CodeActionEntry::Command(command) => {
                return Ok(command_only(action, &definition.name, &command));
            }
        };
        if let Some(reason) = &item.disabled {
            return Ok(with_server(
                done(
                    action,
                    false,
                    format!("Action \"{selected_title}\" is disabled: {reason}"),
                ),
                &definition.name,
            ));
        }
    }
    if item.edit.is_none() && item.command.is_none() {
        return Ok(with_server(
            done(
                action,
                false,
                format!("Action \"{selected_title}\" has no workspace edit or command to apply"),
            ),
            &definition.name,
        ));
    }
    let mut result = match &item.edit {
        Some(edit) => suggestion_result(tools, &client, action, edit, cancellation)?,
        None => {
            return Ok(command_only(
                action,
                &definition.name,
                item.command.as_ref().context("Code action has no edit")?,
            ));
        }
    };
    result["details"]["title"] = json!(selected_title);
    if let Some(command) = item.command {
        result["details"]["command"] = json!(command);
        result["details"]["complete"] = json!(false);
        let content = result["content"].as_str().unwrap_or_default().to_owned();
        result["content"] = json!(format!(
            "{content}\nThis action also requires a server command; the edit may be incomplete. The command was not executed."
        ));
    }
    Ok(result)
}

fn command_only(action: Action, server: &str, command: &Command) -> Value {
    let mut result = with_server(
        done(
            action,
            false,
            format!(
                "{} requires a server command, which LSP will not execute. Use existing tools to implement the intended change.",
                command.title
            ),
        ),
        server,
    );
    result["details"]["applied"] = json!(false);
    result["details"]["command"] = json!(command);
    result
}

#[cfg(test)]
mod tests;
