//! LSP 3.17 protocol types (covering only the parts pp actually negotiates and consumes).
//!
//! Unknown fields are always ignored: language-server extension fields are not an error. Optional/version fields keep
//! the distinction between `null` and absent, because `documentChanges` allows `version: null`.

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
    /// `AnnotatedTextEdit`/snippet extension field: `2` means `newText` is a snippet.
    /// pp refuses to apply snippet edits, because treating them as plain text would insert `${1:...}` placeholders.
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

/// Text document synchronization mode declared by the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncKind {
    None,
    Full,
    Incremental,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SaveKind {
    Never,
    /// The save notification carries the text (`includeText: true`).
    IncludeText,
    /// The save notification carries no text.
    Plain,
}

/// Server capabilities from the `initialize` result.
///
/// `raw` keeps the full capabilities object exactly as the server declared it (including extension
/// fields pp does not know and `executeCommandProvider.commands`); the remaining fields are just convenience
/// views parsed from the **top-level** declarations. In LSP 3.17 the navigation, edit, formatting and execute-command capabilities all live at
/// capabilities top level, and only `workspace.workspaceFolders` is nested; no legacy
/// incorrect-nesting fallback is kept here, to avoid treating undeclared capabilities as available.
#[derive(Clone, Debug)]
pub(crate) struct ServerCapabilities {
    /// The capabilities exactly as returned by the server; `capabilities_json` emits it directly.
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

/// `workspaceFolders.changeNotifications` allows a boolean or a registration id string;
/// both forms mean the server supports workspace-root change notifications.
fn registration(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(_)) => true,
        Some(Value::Object(_)) | Some(Value::Array(_)) => true,
        _ => false,
    }
}

impl ServerCapabilities {
    /// Consume the capabilities returned by initialize.
    ///
    /// Every property is parsed from the LSP 3.17 **top-level** declaration (`workspace.workspaceFolders`
    /// excepted). Parsing only borrows the original value, and finally the whole JSON is moved into [`Self::raw`], avoiding
    /// a second copy of the full capabilities; `diagnostic_provider` keeps only a copy of the options object
    /// for pull-diagnostic decisions.
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
        // The parsing above only borrows the original value; move it in here to avoid a second copy of the full JSON.
        parsed.raw = capabilities;
        parsed
    }
}
