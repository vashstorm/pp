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

#[derive(Default)]
pub(super) struct StreamText {
    pub(super) text: String,
    tail: [u8; 4],
    tail_len: usize,
}

impl StreamText {
    pub(super) fn append(&mut self, mut bytes: &[u8]) -> String {
        let mut delta = String::with_capacity(bytes.len() + self.tail_len);
        while self.tail_len > 0 && !bytes.is_empty() {
            let count = (self.tail.len() - self.tail_len).min(bytes.len());
            self.tail[self.tail_len..self.tail_len + count].copy_from_slice(&bytes[..count]);
            self.tail_len += count;
            bytes = &bytes[count..];
            let used = decode_prefix(&self.tail[..self.tail_len], &mut delta);
            self.tail.copy_within(used..self.tail_len, 0);
            self.tail_len -= used;
        }
        if self.tail_len == 0 {
            let used = decode_prefix(bytes, &mut delta);
            let tail = &bytes[used..];
            self.tail[..tail.len()].copy_from_slice(tail);
            self.tail_len = tail.len();
        }
        self.text.push_str(&delta);
        delta
    }

    pub(super) fn finish(&mut self) -> String {
        let delta = String::from_utf8_lossy(&self.tail[..self.tail_len]).into_owned();
        self.tail_len = 0;
        self.text.push_str(&delta);
        delta
    }
}

// Decode only complete code points/errors. An incomplete suffix needs the next
// read (or EOF), otherwise split multibyte characters would become replacements.
fn decode_prefix(mut bytes: &[u8], output: &mut String) -> usize {
    let total = bytes.len();
    loop {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                output.push_str(text);
                return total;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                output.push_str(
                    std::str::from_utf8(&bytes[..valid]).expect("validated UTF-8 prefix"),
                );
                bytes = &bytes[valid..];
                match error.error_len() {
                    Some(count) => {
                        output.push(char::REPLACEMENT_CHARACTER);
                        bytes = &bytes[count..];
                    }
                    None => return total - bytes.len(),
                }
            }
        }
    }
}
