//! LSP 3.17 协议类型（只覆盖 pp 实际协商与消费的部分）。
//!
//! 未知字段一律忽略：语言服务器的扩展字段不是错误。可选/版本字段保留
//! `null` 与缺失的区别，因为 `documentChanges` 允许 `version: null`。

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct Position {
    pub(crate) line: u32,
    pub(crate) character: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Range {
    pub(crate) start: Position,
    pub(crate) end: Position,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Location {
    pub(crate) uri: String,
    pub(crate) range: Range,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocationLink {
    pub(crate) target_uri: String,
    #[serde(default)]
    pub(crate) target_range: Option<Range>,
    #[serde(default)]
    pub(crate) target_selection_range: Option<Range>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Severity(pub(crate) u8);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiagnosticRelatedInformation {
    pub(crate) location: Location,
    pub(crate) message: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Diagnostic {
    pub(crate) range: Range,
    #[serde(default)]
    pub(crate) severity: Option<Severity>,
    #[serde(default)]
    pub(crate) code: Option<Value>,
    #[serde(default)]
    pub(crate) source: Option<String>,
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) tags: Vec<u32>,
    #[serde(default)]
    pub(crate) related_information: Vec<DiagnosticRelatedInformation>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublishDiagnosticsParams {
    pub(crate) uri: String,
    #[serde(default)]
    pub(crate) diagnostics: Vec<Diagnostic>,
    #[serde(default)]
    pub(crate) version: Option<Option<u64>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TextEdit {
    pub(crate) range: Range,
    pub(crate) new_text: String,
    /// `AnnotatedTextEdit`/snippet 扩展字段：`2` 表示 `newText` 是 snippet。
    /// pp 拒绝应用 snippet 编辑，因为把它们当纯文本会插入 `${1:...}` 占位符。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) insert_text_format: Option<u8>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OptionalVersionedTextDocumentIdentifier {
    pub(crate) uri: String,
    #[serde(default)]
    pub(crate) version: Option<Option<u64>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TextDocumentEdit {
    pub(crate) text_document: OptionalVersionedTextDocumentIdentifier,
    #[serde(default)]
    pub(crate) edits: Vec<TextEdit>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateFileOptions {
    #[serde(default)]
    pub(crate) overwrite: bool,
    #[serde(default)]
    pub(crate) ignore_if_exists: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateFile {
    pub(crate) uri: String,
    #[serde(default)]
    pub(crate) options: Option<CreateFileOptions>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RenameFileOptions {
    #[serde(default)]
    pub(crate) overwrite: bool,
    #[serde(default)]
    pub(crate) ignore_if_exists: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RenameFile {
    pub(crate) old_uri: String,
    pub(crate) new_uri: String,
    #[serde(default)]
    pub(crate) options: Option<RenameFileOptions>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeleteFileOptions {
    #[serde(default)]
    pub(crate) recursive: bool,
    #[serde(default)]
    pub(crate) ignore_if_not_exists: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeleteFile {
    pub(crate) uri: String,
    #[serde(default)]
    pub(crate) options: Option<DeleteFileOptions>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkspaceEdit {
    #[serde(default)]
    pub(crate) changes: Option<std::collections::HashMap<String, Vec<TextEdit>>>,
    #[serde(default)]
    pub(crate) document_changes: Option<Vec<Value>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DocumentSymbol {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) detail: Option<String>,
    #[serde(default)]
    pub(crate) kind: Option<u8>,
    #[serde(default)]
    pub(crate) range: Option<Range>,
    #[serde(default)]
    #[serde(rename = "selectionRange")]
    pub(crate) _selection_range: Option<Range>,
    #[serde(default)]
    pub(crate) children: Vec<DocumentSymbol>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SymbolInformation {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) kind: Option<u8>,
    #[serde(default)]
    pub(crate) container_name: Option<String>,
    pub(crate) location: Location,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Command {
    pub(crate) title: String,
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) arguments: Option<Vec<Value>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodeAction {
    #[serde(default)]
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) kind: Option<String>,
    #[serde(default)]
    pub(crate) disabled: Option<Value>,
    #[serde(default)]
    pub(crate) edit: Option<WorkspaceEdit>,
    #[serde(default)]
    pub(crate) command: Option<Command>,
}

/// 服务器声明的文本文档同步方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncKind {
    None,
    Full,
    Incremental,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SaveKind {
    Never,
    /// 保存通知带正文（`includeText: true`）。
    IncludeText,
    /// 保存通知不带正文。
    Plain,
}

/// `initialize` 结果里的服务器能力。
///
/// `raw` 原样保留服务器声明的完整 capabilities 对象（包括 pp 不认识的扩展
/// 字段以及 `executeCommandProvider.commands`）；其余字段只是从**顶层**声明
/// 解析出的便捷视图。LSP 3.17 的导航、编辑、格式化与执行命令能力都在
/// capabilities 顶层，只有 `workspace.workspaceFolders` 是嵌套的；这里不保留
/// 任何旧的错误嵌套回退，避免把未声明的能力当成可用。
#[derive(Clone, Debug)]
pub(crate) struct ServerCapabilities {
    /// 服务器原样返回的 capabilities；`capabilities_json` 直接输出它。
    pub(crate) raw: Value,
    pub(crate) position_encoding: Option<String>,
    pub(crate) sync: SyncKind,
    pub(crate) save: SaveKind,
    pub(crate) open_close: bool,
    pub(crate) hover: bool,
    pub(crate) definition: bool,
    pub(crate) type_definition: bool,
    pub(crate) implementation: bool,
    pub(crate) references: bool,
    pub(crate) document_symbol: bool,
    pub(crate) workspace_symbol: bool,
    pub(crate) rename: bool,
    pub(crate) prepare_rename: bool,
    pub(crate) code_action: bool,
    pub(crate) code_action_resolve: bool,
    pub(crate) diagnostic_provider: Option<Value>,
    pub(crate) execute_command: bool,
    pub(crate) workspace_folders_change_notifications: bool,
    pub(crate) document_formatting: bool,
}

impl Default for ServerCapabilities {
    fn default() -> Self {
        Self {
            raw: Value::Null,
            position_encoding: None,
            sync: SyncKind::None,
            save: SaveKind::Never,
            open_close: false,
            hover: false,
            definition: false,
            type_definition: false,
            implementation: false,
            references: false,
            document_symbol: false,
            workspace_symbol: false,
            rename: false,
            prepare_rename: false,
            code_action: false,
            code_action_resolve: false,
            diagnostic_provider: None,
            execute_command: false,
            workspace_folders_change_notifications: false,
            document_formatting: false,
        }
    }
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Object(_)) | Some(Value::Array(_)) => true,
        _ => false,
    }
}

/// `workspaceFolders.changeNotifications` 允许布尔值或注册 id 字符串；
/// 两种形式都表示服务器支持工作区根变更通知。
fn registration(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(_)) => true,
        Some(Value::Object(_)) | Some(Value::Array(_)) => true,
        _ => false,
    }
}

impl ServerCapabilities {
    /// 消费 initialize 返回的 capabilities。
    ///
    /// 所有属性都按 LSP 3.17 的**顶层**声明解析（`workspace.workspaceFolders`
    /// 除外）。解析阶段只借用原值，最后把整个 JSON 移入 [`Self::raw`]，不再
    /// 复制一份完整 capabilities；`diagnostic_provider` 只保留选项对象的副本
    /// 供 pull 诊断判定使用。
    pub(crate) fn parse(capabilities: Value) -> Self {
        let mut parsed = Self {
            position_encoding: capabilities
                .get("positionEncoding")
                .and_then(Value::as_str)
                .map(str::to_owned),
            ..Self::default()
        };
        match capabilities.get("textDocumentSync") {
            Some(Value::Number(number)) => {
                parsed.sync = match number.as_u64() {
                    Some(2) => SyncKind::Incremental,
                    Some(1) => SyncKind::Full,
                    _ => SyncKind::None,
                };
                parsed.open_close = parsed.sync != SyncKind::None;
                parsed.save = if parsed.sync == SyncKind::None {
                    SaveKind::Never
                } else {
                    SaveKind::Plain
                };
            }
            Some(Value::Object(sync)) => {
                parsed.sync = match sync.get("change").and_then(Value::as_u64) {
                    Some(2) => SyncKind::Incremental,
                    Some(1) => SyncKind::Full,
                    _ => SyncKind::None,
                };
                parsed.open_close = sync
                    .get("openClose")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                parsed.save = match sync.get("save") {
                    Some(Value::Bool(true)) => SaveKind::Plain,
                    Some(Value::Object(options)) => {
                        if options
                            .get("includeText")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                        {
                            SaveKind::IncludeText
                        } else {
                            SaveKind::Plain
                        }
                    }
                    _ => SaveKind::Never,
                };
            }
            _ => {}
        }
        parsed.hover = truthy(capabilities.get("hoverProvider"));
        parsed.definition = truthy(capabilities.get("definitionProvider"));
        parsed.type_definition = truthy(capabilities.get("typeDefinitionProvider"));
        parsed.implementation = truthy(capabilities.get("implementationProvider"));
        parsed.references = truthy(capabilities.get("referencesProvider"));
        parsed.document_symbol = truthy(capabilities.get("documentSymbolProvider"));
        parsed.workspace_symbol = truthy(capabilities.get("workspaceSymbolProvider"));
        parsed.rename = truthy(capabilities.get("renameProvider"));
        if let Some(Value::Object(rename)) = capabilities.get("renameProvider") {
            parsed.prepare_rename = truthy(rename.get("prepareProvider"));
        }
        if let Some(code_action) = capabilities.get("codeActionProvider") {
            parsed.code_action = truthy(Some(code_action));
            if let Value::Object(options) = code_action {
                parsed.code_action_resolve = options
                    .get("resolveProvider")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            }
        }
        if let Some(provider) = capabilities.get("diagnosticProvider")
            && truthy(Some(provider))
        {
            parsed.diagnostic_provider = Some(provider.clone());
        }
        parsed.execute_command = truthy(capabilities.get("executeCommandProvider"));
        parsed.document_formatting = truthy(capabilities.get("documentFormattingProvider"));
        if let Some(Value::Object(folders)) = capabilities
            .get("workspace")
            .and_then(|workspace| workspace.get("workspaceFolders"))
        {
            parsed.workspace_folders_change_notifications =
                registration(folders.get("changeNotifications"));
        }
        // 以上解析只借用原值；这里整体移入，避免完整 JSON 的第二次复制。
        parsed.raw = capabilities;
        parsed
    }
}
