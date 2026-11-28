//! Synchronization between committed files and the language server's document view.

use crate::Registry;
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
        let mut deny = |request: crate::transport::ServerRequest| {
            request.respond(serde_json::json!({
                "applied": false,
                "failureReason": crate::transport::UNAUTHORIZED_EDIT
            }));
        };
        // Per-file warnings (e.g. content that cannot be synced as UTF-8) and transport/cancellation failures merge into the same report;
        // ordinary tool failures are never swallowed, and never crowd out notifications for the other files in the batch.
        match client.operation(deadline, cancellation, &mut deny, |op| {
            client.sync_file_events_in_operation(op, &events)
        }) {
            Ok(sync_warnings) => warnings.extend(sync_warnings),
            Err(error) => warnings.push(format!(
                "Failed to sync committed files with {}: {error}",
                client.name
            )),
        }
    }
    warnings
}
