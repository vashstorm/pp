//! Content-Length 帧读写、reader/writer 线程与回帧写路径。

use serde_json::{Value, json};
use std::io::{self, BufReader, Read, Write};
use std::sync::Arc;

use super::dispatch::{handle_notification, handle_server_request};
use super::{BODY_LIMIT, Failure, HEADER_LIMIT, NotificationSink, RpcError, Shared};

pub(super) fn writer_loop(
    mut stdin: impl Write,
    frames: std::sync::mpsc::Receiver<Vec<u8>>,
    shared: Arc<Shared>,
) {
    for frame in frames {
        if let Err(error) = stdin.write_all(&frame).and_then(|()| stdin.flush()) {
            shared.note_close(&format!("LSP stdin write failed: {error}"));
            return;
        }
    }
}

/// 读取并分发服务器消息。reader 线程只做解析、内存状态更新与回帧：从不执行
/// 磁盘编辑、不等待 UI；applyEdit 在这里立即拒绝。
pub(super) fn reader_loop(stdout: impl Read, shared: Arc<Shared>, sink: NotificationSink) {
    let mut reader = BufReader::new(stdout);
    loop {
        let body = match read_frame(&mut reader) {
            Ok(body) => body,
            Err(reason) => {
                shared.note_close(&reason);
                return;
            }
        };
        let Ok(message) = serde_json::from_slice::<Value>(&body) else {
            shared.note_close("LSP server sent malformed JSON");
            return;
        };
        let Some(object) = message.as_object() else {
            shared.note_close("LSP server sent a non-object message");
            return;
        };
        let method = object.get("method").and_then(Value::as_str);
        let id = object.get("id").cloned();
        match (method, id) {
            (Some(method), Some(id)) => {
                let params = object.get("params").cloned().unwrap_or(Value::Null);
                handle_server_request(&shared, &sink, method, id, params);
            }
            (Some(method), None) => {
                let params = object.get("params").cloned().unwrap_or(Value::Null);
                handle_notification(&shared, &sink, method, params);
            }
            (None, Some(id)) => {
                let Some(key) = id.as_i64() else {
                    continue;
                };
                let outcome = if let Some(error) = object.get("error") {
                    Err(RpcError {
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("LSP request failed")
                            .to_owned(),
                    })
                } else {
                    Ok(object.get("result").cloned().unwrap_or(Value::Null))
                };
                let sender = shared
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&key);
                if let Some(sender) = sender {
                    let _ = sender.send(outcome);
                }
            }
            (None, None) => {
                shared.note_close("LSP server sent an incomplete message");
                return;
            }
        }
    }
}

pub(super) fn reply_message(
    shared: &Arc<Shared>,
    id: &Value,
    result: Value,
) -> Result<(), Failure> {
    send_raw(
        shared,
        json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
}

pub(super) fn reply_error(
    shared: &Arc<Shared>,
    id: &Value,
    code: i64,
    message: &str,
) -> Result<(), Failure> {
    send_raw(
        shared,
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    )
}

/// reader 线程直接回复 server request 时复用的写路径；写队列由 writer 线程消费，
/// 队列满即视为协议失效（与调用线程的判定一致）。
fn send_raw(shared: &Arc<Shared>, message: Value) -> Result<(), Failure> {
    let body = serde_json::to_vec(&message).map_err(|_| Failure::Closed)?;
    if body.len() > BODY_LIMIT {
        return Err(Failure::Closed);
    }
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    let sender = {
        let guard = shared.writer.lock().unwrap_or_else(|e| e.into_inner());
        guard.clone()
    };
    let Some(sender) = sender else {
        return Err(Failure::Closed);
    };
    match sender.try_send(frame) {
        Ok(()) => Ok(()),
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(Failure::Closed),
        Err(std::sync::mpsc::TrySendError::Full(_)) => {
            shared.note_close("LSP server stopped reading requests");
            Err(Failure::Closed)
        }
    }
}

fn read_frame(reader: &mut impl Read) -> Result<Vec<u8>, String> {
    let headers = read_headers(reader)?;
    let text = String::from_utf8_lossy(&headers);
    let mut length: Option<usize> = None;
    for line in text.split('\n') {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("content-length") {
            continue;
        }
        // 重复 Content-Length 直接判定连接失效：不同值可能是请求走私的雏形，
        // 相同值也没有合法理由出现。
        if length.is_some() {
            return Err("LSP message repeats the Content-Length header".to_owned());
        }
        let value = value.trim();
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("LSP message has an invalid Content-Length header".to_owned());
        }
        let parsed = value
            .parse::<usize>()
            .map_err(|_| "LSP message has an invalid Content-Length header".to_owned())?;
        length = Some(parsed);
    }
    let Some(length) = length else {
        return Err("LSP message is missing a Content-Length header".to_owned());
    };
    if length > BODY_LIMIT {
        return Err("LSP message body exceeds the size limit".to_owned());
    }
    let mut body = vec![0_u8; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| format!("LSP body read failed: {error}"))?;
    Ok(body)
}

fn read_headers(reader: &mut impl Read) -> Result<Vec<u8>, String> {
    let mut headers = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match reader.read_exact(&mut byte) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err("LSP server closed its output".to_owned());
            }
            Err(error) => return Err(format!("LSP header read failed: {error}")),
        }
        headers.push(byte[0]);
        if headers.len() > HEADER_LIMIT {
            return Err("LSP message header exceeds the size limit".to_owned());
        }
        if headers.ends_with(b"\r\n\r\n") || headers.ends_with(b"\n\n") {
            return Ok(headers);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use serde_json::json;
    use std::io::Cursor;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::sync_channel;

    #[test]
    fn lsp_transport_frame_parser_rejects_duplicate_content_length() {
        // 不同值的重复头是请求走私的典型形态，相同值也没有合法理由。
        let mut input = Cursor::new(b"Content-Length: 2\r\nContent-Length: 3\r\n\r\n{}x".to_vec());
        let error = read_frame(&mut input).expect_err("duplicate header must fail");
        assert!(error.contains("repeats the Content-Length"), "{error}");

        let mut identical =
            Cursor::new(b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}".to_vec());
        let error = read_frame(&mut identical).expect_err("duplicate header must fail");
        assert!(error.contains("repeats the Content-Length"), "{error}");
    }

    #[test]
    fn lsp_transport_frame_parser_bounds_and_validates_frames() {
        let body = br#"{"jsonrpc":"2.0"}"#;
        let mut valid = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        valid.extend_from_slice(body);
        assert_eq!(read_frame(&mut Cursor::new(valid)).unwrap(), body.to_vec());

        let mut invalid = Cursor::new(b"Content-Length: abc\r\n\r\n".to_vec());
        let error = read_frame(&mut invalid).expect_err("invalid length must fail");
        assert!(error.contains("invalid Content-Length"), "{error}");

        let mut missing = Cursor::new(b"Content-Type: application/json\r\n\r\n".to_vec());
        let error = read_frame(&mut missing).expect_err("missing length must fail");
        assert!(error.contains("missing a Content-Length"), "{error}");

        let mut oversized =
            Cursor::new(format!("Content-Length: {}\r\n\r\n", BODY_LIMIT + 1).into_bytes());
        let error = read_frame(&mut oversized).expect_err("oversized body must fail");
        assert!(error.contains("exceeds the size limit"), "{error}");

        let mut header_bomb = Cursor::new(vec![b'A'; HEADER_LIMIT + 1]);
        let error = read_frame(&mut header_bomb).expect_err("oversized header must fail");
        assert!(error.contains("header exceeds the size limit"), "{error}");
    }

    #[test]
    fn lsp_transport_reader_dispatches_messages_without_blocking() {
        let (shared, frames) = test_shared();
        let (tx, rx) = sync_channel(1);
        shared.pending.lock().unwrap().insert(5, tx);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let sink: NotificationSink = Arc::new(move |method, _params| {
            recorded.lock().unwrap().push(method.to_owned());
        });
        let mut input = Vec::new();
        input.extend(frame(
            &json!({"jsonrpc": "2.0", "id": 5, "result": {"ok": true}}),
        ));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {},
        })));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "window/showMessageRequest",
            "params": {},
        })));
        reader_loop(Cursor::new(input), shared.clone(), sink);
        assert_eq!(rx.recv().expect("response").unwrap(), json!({"ok": true}));
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["textDocument/publishDiagnostics".to_owned()]
        );
        let reply = read_body(&frames.recv().expect("reply"));
        assert_eq!(reply["id"], json!(9));
        assert_eq!(reply["result"], Value::Null);
        assert!(shared.closed.load(Ordering::SeqCst));
    }

    #[test]
    fn lsp_transport_reader_ignores_responses_for_abandoned_requests() {
        let (shared, _frames) = test_shared();
        let sink = no_sink();
        let input = frame(&json!({"jsonrpc": "2.0", "id": 42, "result": true}));
        reader_loop(Cursor::new(input), shared.clone(), sink);
        // 迟到响应没有等待方：reader 丢弃它并正常结束。
        assert!(shared.pending.lock().unwrap().is_empty());
        assert!(shared.closed.load(Ordering::SeqCst));
    }

    #[test]
    fn lsp_transport_reader_closes_transport_on_malformed_json() {
        let (shared, _frames) = test_shared();
        let (tx, rx) = sync_channel(1);
        shared.pending.lock().unwrap().insert(1, tx);
        let sink = no_sink();
        let input = b"Content-Length: 3\r\n\r\nnot".to_vec();
        reader_loop(Cursor::new(input), shared.clone(), sink);
        assert!(shared.closed.load(Ordering::SeqCst));
        let error = rx.recv().expect("closed pending").unwrap_err();
        assert_eq!(error.code, -32800);
        assert!(shared.pending.lock().unwrap().is_empty());
    }
}
