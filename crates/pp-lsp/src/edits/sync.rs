//! 已提交文件与语言服务器文档视图的同步。

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
        // 逐文件 warning（如无法按 UTF-8 同步的内容）与传输/取消失败合并进同一份报告；
        // 正常工具失败不会被吞掉，也不会挤掉同批其它文件的通知。
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
