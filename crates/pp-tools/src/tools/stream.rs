//! 工具输出事件的即时快照与流式 UTF-8 解码。
use pp_core::events::AgentEvent;
use pp_core::events::OutputStream;
use serde_json::Value;

#[cfg(test)]
use std::{
    io::{self, Read},
    sync::mpsc::SyncSender,
    thread,
};

pub(super) fn emit_background_snapshot(
    call_id: &str,
    snapshot: &Value,
    emit: &mut dyn FnMut(AgentEvent),
) {
    if let Some(shell) = snapshot
        .get("shell")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
    {
        emit(AgentEvent::ToolShellSnapshot {
            id: call_id.to_owned(),
            snapshot: shell,
        });
    }
    if let Some(output) = snapshot["output"].as_str() {
        emit(AgentEvent::ToolOutputSnapshot {
            id: call_id.to_owned(),
            output: output.to_owned(),
        });
        return;
    }
    for (key, stream) in [
        ("stdout", OutputStream::Stdout),
        ("stderr", OutputStream::Stderr),
    ] {
        if let Some(text) = snapshot[key].as_str() {
            emit_output(call_id, stream, text.to_owned(), emit);
        }
    }
}

#[cfg(test)]
pub(super) const PIPE_CHUNK_SIZE: usize = 4096;

#[cfg(test)]
pub(super) fn drain_pipe(
    mut reader: impl Read,
    stream: OutputStream,
    sender: SyncSender<(OutputStream, Vec<u8>)>,
    stop: &std::sync::atomic::AtomicBool,
) -> io::Result<bool> {
    let mut buffer = [0; PIPE_CHUNK_SIZE];
    loop {
        if stop.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(false);
        }
        let count = match reader.read(&mut buffer) {
            Ok(0) => return Ok(false),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            Err(error) => return Err(error),
        };
        if sender.send((stream, buffer[..count].to_vec())).is_err() {
            return Ok(false);
        }
    }
}

pub(super) fn emit_output(
    call_id: &str,
    stream: OutputStream,
    text: String,
    emit: &mut dyn FnMut(AgentEvent),
) {
    if !text.is_empty() {
        emit(AgentEvent::ToolOutput {
            id: call_id.to_owned(),
            stream,
            text,
        });
    }
}
