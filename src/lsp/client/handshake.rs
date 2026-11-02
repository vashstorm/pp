//! initialize 握手、客户端能力声明与 `Failure` → anyhow 转换。

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Instant;

use crate::cancellation::CancellationToken;
use crate::lsp::config::ServerDefinition;
use crate::lsp::transport::{Failure, ServerRequest, Transport, UNAUTHORIZED_EDIT};
use crate::lsp::types::ServerCapabilities;
use crate::lsp::uri::path_to_uri;

pub(super) fn convert_unit(result: Result<(), Failure>, what: &str) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(failure) => Err(failure_to_error(failure, what)),
    }
}

pub(super) fn convert(result: Result<Value, Failure>, what: &str) -> Result<Value> {
    match result {
        Ok(value) => Ok(value),
        Err(Failure::Cancelled) => Err(crate::cancellation::Cancelled.into()),
        Err(Failure::Timeout) => bail!("LSP {what} timed out"),
        Err(Failure::Rpc(code)) => bail!("LSP {what} failed with error code {code}"),
        Err(Failure::Closed) => {
            bail!("LSP {what} failed: the language server connection is closed")
        }
    }
}

/// initialize 握手：协商 UTF-16、保存 initialized 与初始配置推送。
pub(super) fn initialize_handshake(
    transport: &Transport,
    definition: &ServerDefinition,
    cwd: &Path,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<ServerCapabilities> {
    let mut deny = |request: ServerRequest| request.respond(unauthorized_edit());
    let mut service: &mut dyn FnMut(ServerRequest) = &mut deny;
    let result = transport
        .request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "clientInfo": {"name": "pp", "version": env!("CARGO_PKG_VERSION")},
                "rootUri": path_to_uri(cwd),
                "rootPath": cwd.display().to_string(),
                "workspaceFolders": [{
                    "uri": path_to_uri(cwd),
                    "name": cwd.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| cwd.display().to_string()),
                }],
                "capabilities": client_capabilities(),
                "initializationOptions": definition.init_options,
                "trace": "off",
            }),
            deadline,
            cancellation,
            &mut service,
        )
        .map_err(|failure| failure_to_error(failure, "initialize"))?;
    let raw_capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
    // 顶层解析（含 `diagnosticProvider`、`executeCommandProvider` 与
    // `workspace.workspaceFolders.changeNotifications`）都在 ServerCapabilities::parse 里，
    // 这里不再用 raw JSON 打补丁，避免两套能力约定。
    let capabilities = ServerCapabilities::parse(raw_capabilities);
    if let Some(encoding) = capabilities.position_encoding.as_deref()
        && !encoding.eq_ignore_ascii_case("utf-16")
    {
        bail!(
            "language server {} selected unsupported position encoding `{}`",
            definition.name,
            encoding
        );
    }
    convert_unit(transport.notify("initialized", json!({})), "initialized")?;
    convert_unit(
        transport.notify(
            "workspace/didChangeConfiguration",
            json!({"settings": definition.settings}),
        ),
        "workspace/didChangeConfiguration",
    )?;
    Ok(capabilities)
}

pub(super) fn unauthorized_edit() -> Value {
    json!({"applied": false, "failureReason": UNAUTHORIZED_EDIT})
}

fn failure_to_error(failure: Failure, what: &str) -> anyhow::Error {
    match failure {
        Failure::Cancelled => crate::cancellation::Cancelled.into(),
        Failure::Timeout => anyhow::anyhow!("LSP {what} timed out"),
        Failure::Rpc(code) => anyhow::anyhow!("LSP {what} failed with error code {code}"),
        Failure::Closed => anyhow::anyhow!("LSP {what} failed: the connection is closed"),
    }
}

/// 初始化失败时的收尾：回收成功才返回原始错误；回收失败时保留
/// `CleanupError`（registry 据此不写负缓存并可重试 slot），同时保留初始化原因文本。
pub(super) fn startup_failure(startup: anyhow::Error, cleanup: Result<()>) -> anyhow::Error {
    match cleanup {
        Ok(()) => startup,
        Err(cleanup) => {
            let cleanup: anyhow::Error = crate::cancellation::CleanupError(cleanup).into();
            cleanup.context(format!("language server failed to start: {startup:#}"))
        }
    }
}

pub(super) fn is_rust_analyzer(command: &Path) -> bool {
    command
        .file_name()
        .map(|name| {
            let name = name.to_string_lossy().to_ascii_lowercase();
            name == "rust-analyzer" || name == "rust-analyzer.exe"
        })
        .unwrap_or(false)
}

fn client_capabilities() -> Value {
    json!({
        "general": {
            "positionEncodings": ["utf-16"],
            "markdown": {"parser": "none"},
        },
        "textDocument": {
            "synchronization": {
                "dynamicRegistration": false,
                "willSave": false,
                "willSaveWaitUntil": false,
                "didSave": true,
            },
            "definition": {"dynamicRegistration": false, "linkSupport": true},
            "typeDefinition": {"dynamicRegistration": false, "linkSupport": true},
            "implementation": {"dynamicRegistration": false, "linkSupport": true},
            "references": {"dynamicRegistration": false},
            "hover": {"dynamicRegistration": false, "contentFormat": ["markdown", "plaintext"]},
            "documentSymbol": {
                "dynamicRegistration": false,
                "hierarchicalDocumentSymbolSupport": true,
            },
            "rename": {"dynamicRegistration": false, "prepareSupport": true},
            "codeAction": {
                "dynamicRegistration": false,
                "codeActionLiteralSupport": {
                    "codeActionKind": {
                        "valueSet": [
                            "quickfix",
                            "refactor",
                            "refactor.extract",
                            "refactor.inline",
                            "refactor.rewrite",
                            "source",
                            "source.organizeImports",
                            "source.fixAll",
                        ],
                    },
                },
                "resolveSupport": {"properties": ["edit"]},
            },
            "publishDiagnostics": {
                "relatedInformation": true,
                "versionSupport": true,
                "tagSupport": {"valueSet": [1, 2]},
                "codeDescriptionSupport": true,
                "dataSupport": true,
            },
            "diagnostic": {"dynamicRegistration": true},
        },
        "window": {"workDoneProgress": true},
        "workspace": {
            "applyEdit": false,
            "workspaceEdit": {
                "documentChanges": true,
                "resourceOperations": ["create", "rename", "delete"],
                            },
            "configuration": true,
            "workspaceFolders": true,
            "symbol": {"dynamicRegistration": false},
            "fileOperations": {
                "dynamicRegistration": false,
                "willCreate": false,
                "didCreate": false,
                "willRename": true,
                "didRename": false,
                "willDelete": false,
                "didDelete": false,
            },
        },
    })
}
