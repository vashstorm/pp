//! `lsp` 工具：14 个动作的严格解析、动作级门禁与同步执行。
//!
//! 门禁分两层且彼此独立：工具定义按 `Access` 裁剪（只读会话只暴露只读动作），
//! `validate_access` 与 [`run`] 在参数解析后再次校验，隐藏 schema 永远不是
//! 授权边界。`run` 返回 pp 工具结果（`ok`/`content`/`details`）；取消与清理
//! 失败保留 typed error 向上传播，普通动作失败用 `ok:false` 的完整结果说明。
//!
//! 所有编辑仅用于建议；服务器 applyEdit 一律拒绝，工具从不提交文件修改。

mod code_actions;
mod diagnostics;
mod format;
mod inspect;
mod navigation;
mod position;
mod rename;
mod rename_file;
mod servers;
mod suggestion;
mod symbols;

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use async_openai::types::chat::FunctionObject;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::lsp::Registry;
use crate::lsp::adapters;
use crate::lsp::config::ServerDefinition;
use crate::lsp::transport::Failure;
use crate::lsp::transport::ServerRequest;
use crate::lsp::transport::UNAUTHORIZED_EDIT;
use crate::tools::Tools;
use crate::tools::approval::Tier;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;
use pp_core::cancellation::CleanupError;
use pp_core::cancellation::check;
use pp_core::events::AgentEvent;

use code_actions::code_actions;
use diagnostics::diagnostics_action;
use inspect::{capabilities, reload, status};
use navigation::navigation;
use rename::rename;
use rename_file::rename_file;
use suggestion::format_suggestions;
use symbols::symbols;

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
        r#"Query language servers for semantic code navigation, diagnostics and modification suggestions.
Use symbols with file="*" and query to find a symbol when its file is unknown; use symbols with a concrete file for its outline. At a known file/line/symbol, use definition to locate the declaration, references to inspect usages before changing an API, type_definition or hover to understand types, and implementation to find implementations.
Use diagnostics for errors in a file or bounded glob; file="*" is unsupported for diagnostics. Use bash for project checks. Use rename for symbol rename suggestions, rename_file for file/directory move suggestions, code_actions for fixes/refactorings, and format for formatting suggestions. code_actions first lists candidates; repeat with the same file/line/query and selection to resolve one.
Examples (replace paths, lines and names with observed code):
{"action":"symbols","file":"*","query":"ModelCatalog"}
{"action":"references","file":"src/main.rs","line":42,"symbol":"run"}
{"action":"code_actions","file":"src/main.rs","line":42,"query":"quickfix"}
LSP never applies edits or executes server commands. Read target files again and use edit/write/bash to implement suggestions, then verify. Input lines are 1-based; original LSP ranges in suggestions are zero-based UTF-16. Servers are not a filesystem sandbox. Use status to investigate availability without starting servers, capabilities to inspect supported operations, and reload after fixing configuration or server failures."#,
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
                "action": {
                    "type": "string", "enum": actions,
                    "description": "Operation to perform. symbols discovers names/structure; definition, references, type_definition, implementation and hover navigate a known position; diagnostics checks files; rename, rename_file, code_actions and format return suggestions only; status, capabilities and reload inspect/manage servers."
                },
                "file": {
                    "type": "string",
                    "description": "Path relative to startup cwd, or an absolute path. Required for navigation, symbols, diagnostics and suggestions. symbols: use a concrete file for its outline, or '*' with query for workspace symbol search. diagnostics: a concrete file or bounded glob such as 'src/**/*.rs' (at most 20 files), never '*'. rename_file: existing source file/directory. capabilities/reload: optional concrete file to select covering servers; omit or use '*' for all discovered servers. status needs no file."
                },
                "line": {
                    "type": "integer", "minimum": 1,
                    "description": "1-based source line for definition, type_definition, implementation, references, hover, rename or code_actions. Read the file or use returned symbol locations to obtain it. Defaults to 1; supply the actual line for position-based queries."
                },
                "symbol": {
                    "type": "string",
                    "description": "Text identifying the target on the specified line; not a workspace search. Use name#N for its Nth occurrence on that line (1-based), e.g. 'run#2'. Supply with line for navigation and symbol rename; required when line is supplied for project-aware Rust definition/references/rename. If omitted, the position is the first non-whitespace character."
                },
                "query": {
                    "type": "string",
                    "description": "Required non-empty symbol search text for symbols with file='*'. For code_actions, an optional action-kind filter such as 'quickfix', 'refactor' or 'source.organizeImports'; use selection for a candidate title. Does not filter a concrete file's symbol outline."
                },
                "new_name": {
                    "type": "string",
                    "description": "Required for rename (new symbol identifier) and rename_file (destination file/directory path, relative to startup cwd or absolute). Returns suggestions without renaming or moving anything."
                },
                "timeout": {
                    "type": "number", "minimum": 5, "maximum": 300,
                    "description": "Override the total call wait budget in seconds (5..300), including server startup/indexing. If omitted, ordinary actions use at least 20 seconds and diagnostics uses the configured requestTimeout."
                },
                "selection": {
                    "type": "string",
                    "description": "code_actions only: select from a previous candidate list by 0-based index encoded as a string (e.g. '0') or a title substring. Repeat the original file/line/symbol/query so the candidate list is comparable. Omit to list candidates; selecting returns a resolved suggestion without applying it."
                }
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

#[cfg(test)]
mod tests;
