//! 服务器通知与 server request 的分发。

use serde_json::{Value, json};
use std::sync::Arc;

use super::io::{reply_error, reply_message};
use super::{NotificationSink, Shared, UNAUTHORIZED_EDIT};

pub(super) fn handle_notification(
    _shared: &Arc<Shared>,
    sink: &NotificationSink,
    method: &str,
    params: Value,
) {
    sink(method, params);
}

pub(super) fn handle_server_request(
    shared: &Arc<Shared>,
    sink: &NotificationSink,
    method: &str,
    id: Value,
    params: Value,
) {
    let reply_id = id.clone();
    let reply = move |result: Value| {
        let _ = reply_message(shared, &reply_id, result);
    };
    match method {
        "workspace/applyEdit" => {
            reply(json!({"applied": false, "failureReason": UNAUTHORIZED_EDIT}));
        }
        "workspace/configuration" => {
            let settings = shared
                .settings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let items = params
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let answers: Vec<Value> = items
                .iter()
                .map(|item| {
                    let Some(section) = item.get("section").and_then(Value::as_str) else {
                        return Value::Null;
                    };
                    let mut current = &settings;
                    for part in section.split('.') {
                        match current.get(part) {
                            Some(next) => current = next,
                            None => return Value::Null,
                        }
                    }
                    current.clone()
                })
                .collect();
            reply(Value::Array(answers));
        }
        "workspace/workspaceFolders" => {
            let folders = shared
                .folders
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            reply(if folders.is_empty() {
                Value::Null
            } else {
                Value::Array(folders)
            });
        }
        "client/registerCapability" => {
            if let Some(registrations) = params.get("registrations").and_then(Value::as_array) {
                shared.register_capabilities(registrations);
            }
            reply(Value::Null);
        }
        "client/unregisterCapability" => {
            // LSP 规范的字段名是 `unregisterations`；部分实现写成
            // `unregistrations`，与上游一致地两种都接受。
            let entries = params
                .get("unregisterations")
                .or_else(|| params.get("unregistrations"))
                .and_then(Value::as_array);
            if let Some(entries) = entries {
                shared.unregister_capabilities(entries);
            }
            reply(Value::Null);
        }
        "window/showMessageRequest" => reply(Value::Null),
        "window/showDocument" => reply(json!({"success": false})),
        "window/workDoneProgress/create" => reply(Value::Null),
        "workspace/diagnostic/refresh"
        | "workspace/codeLens/refresh"
        | "workspace/semanticTokens/refresh"
        | "workspace/inlayHint/refresh"
        | "workspace/inlineValue/refresh" => {
            sink(method, params);
            reply(Value::Null);
        }
        _ => {
            let _ = reply_error(shared, &id, -32601, "method not supported");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use serde_json::json;
    use std::sync::atomic::Ordering;

    #[test]
    fn lsp_transport_dynamic_registration_survives_unregister_of_other_ids() {
        let (shared, frames) = test_shared();
        let sink = no_sink();
        handle_server_request(
            &shared,
            &sink,
            "client/registerCapability",
            json!(1),
            json!({"registrations": [
                {"id": "diag", "method": "textDocument/diagnostic"},
                {"id": "watch", "method": "workspace/didChangeWatchedFiles"},
            ]}),
        );
        assert!(shared.diagnostics_registered());
        handle_server_request(
            &shared,
            &sink,
            "client/unregisterCapability",
            json!(2),
            json!({"unregisterations": [
                {"id": "watch", "method": "workspace/didChangeWatchedFiles"},
            ]}),
        );
        // 一个 unregister 不能清掉同 method 的其他注册。
        assert!(shared.diagnostics_registered());
        handle_server_request(
            &shared,
            &sink,
            "client/unregisterCapability",
            json!(3),
            json!({"unregistrations": [
                {"id": "diag", "method": "textDocument/diagnostic"},
            ]}),
        );
        assert!(!shared.diagnostics_registered());
        for _ in 0..3 {
            assert_eq!(
                read_body(&frames.recv().expect("reply"))["result"],
                Value::Null
            );
        }
    }

    #[test]
    fn lsp_transport_dynamic_registration_replaces_method_for_same_id() {
        let (shared, _frames) = test_shared();
        let sink = no_sink();
        handle_server_request(
            &shared,
            &sink,
            "client/registerCapability",
            json!(1),
            json!({"registrations": [{"id": "diag", "method": "textDocument/diagnostic"}]}),
        );
        assert!(shared.diagnostics_registered());
        handle_server_request(
            &shared,
            &sink,
            "client/registerCapability",
            json!(2),
            json!({"registrations": [{"id": "diag", "method": "workspace/didChangeWatchedFiles"}]}),
        );
        // 同 id 重新注册是替换，不是叠加。
        assert!(!shared.diagnostics_registered());
        handle_server_request(
            &shared,
            &sink,
            "client/unregisterCapability",
            json!(3),
            json!({"unregisterations": [{"id": "diag", "method": "workspace/didChangeWatchedFiles"}]}),
        );
        assert!(shared.registrations.lock().unwrap().is_empty());
    }

    #[test]
    fn lsp_transport_apply_edit_is_rejected_when_no_operation_waits() {
        let (shared, frames) = test_shared();
        let sink = no_sink();
        handle_server_request(
            &shared,
            &sink,
            "workspace/applyEdit",
            json!(7),
            json!({"edit": {}}),
        );
        let reply = read_body(&frames.recv().expect("reply"));
        assert_eq!(reply["id"], json!(7));
        assert_eq!(reply["result"]["applied"], json!(false));
        assert_eq!(reply["result"]["failureReason"], json!(UNAUTHORIZED_EDIT));
        assert!(shared.callbacks.lock().unwrap().is_empty());
    }

    #[test]
    fn lsp_transport_apply_edit_is_refused_during_active_operation() {
        let (shared, frames) = test_shared();
        shared.active.store(1, Ordering::SeqCst);
        handle_server_request(
            &shared,
            &no_sink(),
            "workspace/applyEdit",
            json!(11),
            json!({"edit":{}}),
        );
        let reply = read_body(&frames.recv().unwrap());
        assert_eq!(reply["result"]["applied"], false);
        assert!(shared.callbacks.lock().unwrap().is_empty());
    }
}
