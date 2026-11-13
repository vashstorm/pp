//! 已提交文件与语言服务器文档视图的同步。

use crate::lsp::Registry;
use pp_core::cancellation::CancellationToken;
use std::path::PathBuf;
use std::time::Instant;

pub(crate) fn sync_file_events(
    registry: &Registry,
    events: &[(PathBuf, u8)],
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for client in registry.started() {
        let events: Vec<_> = events
            .iter()
            .filter_map(|(path, kind)| {
                let path = if path.is_absolute() {
                    path.clone()
                } else {
                    registry.cwd().join(path)
                };
                (registry.covers(&client, &path) || client.document(&path).is_some())
                    .then_some((path, *kind))
            })
            .collect();
        if events.is_empty() {
            continue;
        }
        let mut deny = |request: crate::lsp::transport::ServerRequest| {
            request.respond(serde_json::json!({
                "applied": false,
                "failureReason": crate::lsp::transport::UNAUTHORIZED_EDIT
            }));
        };
        if let Err(error) = client.operation(deadline, cancellation, &mut deny, |op| {
            client.sync_file_events_in_operation(op, &events)
        }) {
            warnings.push(format!(
                "Failed to sync committed files with {}: {error}",
                client.name
            ));
        }
    }
    warnings
}
