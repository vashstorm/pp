//! Responses API 的 SSE 编码：单一生命周期、原生 item 事件与可取消的 token 限速。

use std::{
    borrow::Cow,
    io::Write,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use pp_core::cancellation::CancellationToken;
use serde_json::{Value, json};
use tiny_http::Request;

use super::protocol::{self, Instruction, Text, Tool};

/// 限速等待期间检查取消的最大间隔。
const WAKE_SLICE: Duration = Duration::from_millis(10);

/// 将一个已预留的指令组编码为单个 Responses SSE 响应。
///
/// 队列的 busy 标记与取消由调用方持有；这里只负责在分片之间与限速等待中
/// 检查取消，并在写入失败时立即停止。
pub(super) fn send(
    request: Request,
    instructions: &[Instruction],
    identity: &str,
    lane: &str,
    shutdown: &CancellationToken,
    run_cancel: &CancellationToken,
) -> Result<()> {
    let usage = protocol::group_usage(instructions)?;
    let mut stream = Stream::open(request)?;
    if cancelled(shutdown, run_cancel) {
        return stream.abort();
    }
    let id = format!("resp-{identity}");
    let model = format!("pp-mock-{lane}");
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    stream.event(
        "response.created",
        json!({"response": response_body(&id, &model, created_at, "in_progress", &[])}),
    )?;
    stream.event(
        "response.in_progress",
        json!({"response": response_body(&id, &model, created_at, "in_progress", &[])}),
    )?;
    let mut turn = Turn {
        stream,
        identity,
        output_index: 0,
        output: Vec::new(),
        shutdown,
        run_cancel,
    };
    for (step, instruction) in instructions.iter().enumerate() {
        if !turn.running() {
            return turn.stream.abort();
        }
        let step = u64::try_from(step).context("mock AI script is too long")?;
        let mut throttle = Throttle::new(instruction.tokens_per_second)?;
        let think = full_text(&instruction.think);
        if !think.is_empty() && !turn.reasoning(step, &think, &instruction.think, &mut throttle)? {
            return turn.stream.abort();
        }
        let content = full_text(&instruction.content);
        if !content.is_empty()
            && !turn.message(
                "msg",
                step,
                &content,
                Body::Text(&instruction.content),
                &mut throttle,
            )?
        {
            return turn.stream.abort();
        }
        if let Some(echo) = echo_text(instruction.response.as_ref())? {
            let echo = echo.as_ref();
            if !turn.message("echo", step, echo, Body::Plain(echo), &mut throttle)? {
                return turn.stream.abort();
            }
        }
        if let Some(tool) = instruction.tool.as_ref()
            && !turn.function_call(step, tool, &mut throttle)?
        {
            return turn.stream.abort();
        }
    }
    if !turn.running() {
        return turn.stream.abort();
    }
    let mut response = response_body(&id, &model, created_at, "completed", &turn.output);
    response["usage"] = usage.unwrap_or(Value::Null);
    turn.stream
        .event("response.completed", json!({"response": response}))?;
    turn.stream.finish()
}

fn response_body(id: &str, model: &str, created_at: u64, status: &str, output: &[Value]) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": created_at,
        "model": model,
        "status": status,
        "output": output,
    })
}

/// 正文全文：字符串直接借用，显式分片仅在需要全文时拼接一次。
fn full_text(text: &Text) -> Cow<'_, str> {
    match text {
        Text::String(text) => Cow::Borrowed(text),
        Text::Chunks(_) => Cow::Owned(text.joined()),
    }
}

/// `response` 只做数据回显：字符串逐字输出，其他非 null 值使用紧凑 JSON。
fn echo_text(response: Option<&Value>) -> Result<Option<Cow<'_, str>>> {
    match response {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(Cow::Borrowed(text))),
        Some(value) => Ok(Some(Cow::Owned(
            serde_json::to_string(value).context("serializing mock response echo")?,
        ))),
    }
}

struct Turn<'a> {
    stream: Stream,
    identity: &'a str,
    output_index: u32,
    output: Vec<Value>,
    shutdown: &'a CancellationToken,
    run_cancel: &'a CancellationToken,
}

impl Turn<'_> {
    fn running(&self) -> bool {
        !cancelled(self.shutdown, self.run_cancel)
    }

    fn item_id(&self, kind: &str, step: u64) -> String {
        format!("{kind}-{}-{step}", self.identity)
    }

    fn take_index(&mut self) -> Result<u32> {
        let index = self.output_index;
        self.output_index = index
            .checked_add(1)
            .context("mock AI output index overflow")?;
        Ok(index)
    }

    fn reasoning(
        &mut self,
        step: u64,
        text: &str,
        body: &Text,
        throttle: &mut Throttle,
    ) -> Result<bool> {
        let item_id = self.item_id("reasoning", step);
        let index = self.take_index()?;
        self.stream.event(
            "response.output_item.added",
            json!({
                "output_index": index,
                "item": {
                    "type": "reasoning",
                    "id": item_id,
                    "summary": [],
                    "status": "in_progress"
                }
            }),
        )?;
        self.stream.event(
            "response.reasoning_summary_part.added",
            json!({
                "item_id": item_id,
                "output_index": index,
                "summary_index": 0,
                "part": {"type": "summary_text", "text": ""}
            }),
        )?;
        let mut emit = |stream: &mut Stream, delta: &str| {
            stream.event(
                "response.reasoning_summary_text.delta",
                json!({
                    "item_id": item_id,
                    "output_index": index,
                    "summary_index": 0,
                    "delta": delta
                }),
            )
        };
        if !emit_body(
            &mut self.stream,
            throttle,
            Body::Text(body),
            self.shutdown,
            self.run_cancel,
            &mut emit,
        )? {
            return Ok(false);
        }
        if !self.running() {
            return Ok(false);
        }
        self.stream.event(
            "response.reasoning_summary_text.done",
            json!({
                "item_id": item_id,
                "output_index": index,
                "summary_index": 0,
                "text": text
            }),
        )?;
        let part = json!({"type": "summary_text", "text": text});
        self.stream.event(
            "response.reasoning_summary_part.done",
            json!({
                "item_id": item_id,
                "output_index": index,
                "summary_index": 0,
                "part": part
            }),
        )?;
        let item = json!({
            "type": "reasoning",
            "id": item_id,
            "summary": [part],
            "status": "completed"
        });
        self.stream.event(
            "response.output_item.done",
            json!({"output_index": index, "item": item}),
        )?;
        self.output.push(item);
        Ok(self.running())
    }

    fn message(
        &mut self,
        kind: &str,
        step: u64,
        text: &str,
        body: Body<'_>,
        throttle: &mut Throttle,
    ) -> Result<bool> {
        if text.is_empty() {
            return Ok(self.running());
        }
        let item_id = self.item_id(kind, step);
        let index = self.take_index()?;
        self.stream.event(
            "response.output_item.added",
            json!({
                "output_index": index,
                "item": {
                    "type": "message",
                    "id": item_id,
                    "role": "assistant",
                    "status": "in_progress",
                    "content": []
                }
            }),
        )?;
        self.stream.event(
            "response.content_part.added",
            json!({
                "item_id": item_id,
                "output_index": index,
                "content_index": 0,
                "part": output_text("")
            }),
        )?;
        let mut emit = |stream: &mut Stream, delta: &str| {
            stream.event(
                "response.output_text.delta",
                json!({
                    "item_id": item_id,
                    "output_index": index,
                    "content_index": 0,
                    "delta": delta
                }),
            )
        };
        if !emit_body(
            &mut self.stream,
            throttle,
            body,
            self.shutdown,
            self.run_cancel,
            &mut emit,
        )? {
            return Ok(false);
        }
        if !self.running() {
            return Ok(false);
        }
        self.stream.event(
            "response.output_text.done",
            json!({
                "item_id": item_id,
                "output_index": index,
                "content_index": 0,
                "text": text,
                "logprobs": null
            }),
        )?;
        self.stream.event(
            "response.content_part.done",
            json!({
                "item_id": item_id,
                "output_index": index,
                "content_index": 0,
                "part": output_text(text)
            }),
        )?;
        let item = json!({
            "type": "message",
            "id": item_id,
            "role": "assistant",
            "status": "completed",
            "content": [output_text(text)]
        });
        self.stream.event(
            "response.output_item.done",
            json!({"output_index": index, "item": item}),
        )?;
        self.output.push(item);
        Ok(self.running())
    }

    fn function_call(&mut self, step: u64, tool: &Tool, throttle: &mut Throttle) -> Result<bool> {
        let arguments =
            serde_json::to_string(&tool.arguments).context("serializing mock tool arguments")?;
        let item_id = self.item_id("fc", step);
        let call_id = format!("call-{}-{step}", self.identity);
        let index = self.take_index()?;
        self.stream.event(
            "response.output_item.added",
            json!({
                "output_index": index,
                "item": {
                    "type": "function_call",
                    "id": item_id,
                    "call_id": call_id,
                    "name": tool.name,
                    "arguments": "",
                    "status": "in_progress"
                }
            }),
        )?;
        let mut emit = |stream: &mut Stream, delta: &str| {
            stream.event(
                "response.function_call_arguments.delta",
                json!({
                    "item_id": item_id,
                    "output_index": index,
                    "delta": delta
                }),
            )
        };
        if !emit_body(
            &mut self.stream,
            throttle,
            Body::Plain(&arguments),
            self.shutdown,
            self.run_cancel,
            &mut emit,
        )? {
            return Ok(false);
        }
        if !self.running() {
            return Ok(false);
        }
        self.stream.event(
            "response.function_call_arguments.done",
            json!({
                "item_id": item_id,
                "output_index": index,
                "name": tool.name,
                "arguments": arguments
            }),
        )?;
        let item = json!({
            "type": "function_call",
            "id": item_id,
            "call_id": call_id,
            "name": tool.name,
            "arguments": arguments,
            "status": "completed"
        });
        self.stream.event(
            "response.output_item.done",
            json!({"output_index": index, "item": item}),
        )?;
        self.output.push(item);
        Ok(self.running())
    }
}

fn output_text(text: &str) -> Value {
    json!({"type": "output_text", "text": text, "annotations": [], "logprobs": null})
}

/// 正文来源：显式分片或普通字符串（回显、工具参数）。
enum Body<'a> {
    Text(&'a Text),
    Plain(&'a str),
}

/// 按 mock token 逐个写出；返回 `false` 表示取消发生在写出之前。
fn emit_body(
    stream: &mut Stream,
    throttle: &mut Throttle,
    body: Body<'_>,
    shutdown: &CancellationToken,
    run_cancel: &CancellationToken,
    emit: &mut dyn FnMut(&mut Stream, &str) -> Result<()>,
) -> Result<bool> {
    match body {
        Body::Text(Text::Chunks(chunks)) => {
            for chunk in chunks {
                // 显式空元素不是 mock token，也不产生空帧。
                if chunk.is_empty() {
                    continue;
                }
                if !throttle.ready(shutdown, run_cancel)? {
                    return Ok(false);
                }
                emit(stream, chunk)?;
            }
            Ok(true)
        }
        Body::Text(Text::String(text)) => {
            emit_plain(stream, throttle, text, shutdown, run_cancel, emit)
        }
        Body::Plain(text) => emit_plain(stream, throttle, text, shutdown, run_cancel, emit),
    }
}

/// 不限速字符串整段一帧；限速时按 Unicode scalar 分片，不按 UTF-8 字节切割。
fn emit_plain(
    stream: &mut Stream,
    throttle: &mut Throttle,
    text: &str,
    shutdown: &CancellationToken,
    run_cancel: &CancellationToken,
    emit: &mut dyn FnMut(&mut Stream, &str) -> Result<()>,
) -> Result<bool> {
    if text.is_empty() {
        return Ok(true);
    }
    if !throttle.limited() {
        if !throttle.ready(shutdown, run_cancel)? {
            return Ok(false);
        }
        emit(stream, text)?;
        return Ok(true);
    }
    let mut buffer = [0; 4];
    for character in text.chars() {
        if !throttle.ready(shutdown, run_cancel)? {
            return Ok(false);
        }
        emit(stream, character.encode_utf8(&mut buffer))?;
    }
    Ok(true)
}

fn cancelled(shutdown: &CancellationToken, run_cancel: &CancellationToken) -> bool {
    shutdown.is_cancelled() || run_cancel.is_cancelled()
}

/// 每条指令独立的单调时钟限速；首个 token 立即发送。
struct Throttle {
    interval: Option<Duration>,
    next: Option<Instant>,
}

impl Throttle {
    fn new(tokens_per_second: f64) -> Result<Self> {
        if tokens_per_second == 0.0 {
            return Ok(Self {
                interval: None,
                next: None,
            });
        }
        ensure!(
            tokens_per_second.is_finite() && tokens_per_second > 0.0,
            "mock AI token rate is invalid"
        );
        let interval = Duration::try_from_secs_f64(1.0 / tokens_per_second)
            .map_err(|_| anyhow::anyhow!("mock AI token rate is not representable"))?;
        Ok(Self {
            interval: Some(interval),
            next: None,
        })
    }

    fn limited(&self) -> bool {
        self.interval.is_some()
    }

    /// 等到下一个 mock token 的 deadline；`false` 表示等待期间被取消。
    fn ready(
        &mut self,
        shutdown: &CancellationToken,
        run_cancel: &CancellationToken,
    ) -> Result<bool> {
        if cancelled(shutdown, run_cancel) {
            return Ok(false);
        }
        let Some(interval) = self.interval else {
            return Ok(true);
        };
        let Some(deadline) = self.next else {
            self.next = Some(
                Instant::now()
                    .checked_add(interval)
                    .context("mock AI stream deadline overflow")?,
            );
            return Ok(true);
        };
        loop {
            if cancelled(shutdown, run_cancel) {
                return Ok(false);
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            thread::sleep(deadline.duration_since(now).min(WAKE_SLICE));
        }
        self.next = Some(
            deadline
                .checked_add(interval)
                .context("mock AI stream deadline overflow")?,
        );
        Ok(true)
    }
}

/// chunked SSE 单帧写入与递增的 `sequence_number`。
struct Stream {
    writer: Box<dyn Write + Send>,
    sequence: u64,
}

impl Stream {
    fn open(request: Request) -> Result<Self> {
        let mut stream = Self::with_writer(request.into_writer());
        stream
            .writer
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .context("starting mock AI SSE response")?;
        stream
            .writer
            .flush()
            .context("flushing mock AI SSE headers")?;
        Ok(stream)
    }

    fn with_writer(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer,
            sequence: 0,
        }
    }

    fn event(&mut self, kind: &str, mut data: Value) -> Result<()> {
        let object = data
            .as_object_mut()
            .context("mock AI SSE event payload must be an object")?;
        object.insert("type".to_owned(), Value::String(kind.to_owned()));
        object.insert("sequence_number".to_owned(), Value::from(self.sequence));
        self.sequence = self.sequence.saturating_add(1);
        let frame = format!("event: {kind}\ndata: {data}\n\n");
        write!(self.writer, "{:x}\r\n", frame.len()).context("writing mock AI SSE chunk header")?;
        self.writer
            .write_all(frame.as_bytes())
            .context("writing mock AI SSE chunk")?;
        self.writer
            .write_all(b"\r\n")
            .context("writing mock AI SSE chunk terminator")?;
        self.writer.flush().context("flushing mock AI SSE chunk")?;
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.writer
            .write_all(b"0\r\n\r\n")
            .context("finishing mock AI SSE response")?;
        self.writer
            .flush()
            .context("flushing mock AI SSE response")?;
        Ok(())
    }

    /// 取消或断开时结束 chunked body，但不发送 `response.completed`。
    fn abort(self) -> Result<()> {
        self.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io,
        sync::{Arc, Mutex},
    };

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn stream() -> (Stream, Sink) {
        let sink = Sink::default();
        (Stream::with_writer(Box::new(sink.clone())), sink)
    }

    /// 只解析 chunked framing，返回每个完整 SSE 帧。
    fn dechunk(bytes: &[u8]) -> Vec<String> {
        let mut rest = bytes;
        let mut frames = Vec::new();
        loop {
            let end = rest
                .windows(2)
                .position(|window| window == b"\r\n")
                .expect("chunk header terminator");
            let size =
                usize::from_str_radix(std::str::from_utf8(&rest[..end]).unwrap(), 16).unwrap();
            rest = &rest[end + 2..];
            if size == 0 {
                assert_eq!(rest, b"\r\n", "invalid final chunk terminator");
                break;
            }
            frames.push(String::from_utf8(rest[..size].to_vec()).unwrap());
            assert_eq!(&rest[size..size + 2], b"\r\n".as_slice());
            rest = &rest[size + 2..];
        }
        frames
    }

    #[test]
    fn mock_sse_frames_are_chunked_and_sequenced() {
        let (mut stream, sink) = stream();
        stream
            .event("response.created", json!({"response": {"id": "resp"}}))
            .unwrap();
        stream
            .event("response.in_progress", json!({"response": {"id": "resp"}}))
            .unwrap();
        stream.finish().unwrap();
        let bytes = sink.0.lock().unwrap().clone();
        let frames = dechunk(&bytes);
        assert_eq!(frames.len(), 2);
        let created = frames[0]
            .strip_prefix("event: response.created\ndata: ")
            .unwrap();
        let created = created.strip_suffix("\n\n").unwrap();
        let created: Value = serde_json::from_str(created).unwrap();
        assert_eq!(created["type"], "response.created");
        assert_eq!(created["sequence_number"], 0);
        let progress = frames[1]
            .strip_prefix("event: response.in_progress\ndata: ")
            .unwrap();
        let progress = progress.strip_suffix("\n\n").unwrap();
        let progress: Value = serde_json::from_str(progress).unwrap();
        assert_eq!(progress["sequence_number"], 1);
    }

    fn tokens(body: Body<'_>, rate: f64) -> Vec<String> {
        let mut stream = Stream::with_writer(Box::new(io::sink()));
        let mut throttle = Throttle::new(rate).unwrap();
        let shutdown = CancellationToken::new();
        let run_cancel = CancellationToken::new();
        let mut collected = Vec::new();
        let mut emit = |_: &mut Stream, delta: &str| {
            collected.push(delta.to_owned());
            Ok(())
        };
        assert!(
            emit_body(
                &mut stream,
                &mut throttle,
                body,
                &shutdown,
                &run_cancel,
                &mut emit
            )
            .unwrap()
        );
        collected
    }

    #[test]
    fn mock_sse_tokens_follow_rate_and_chunk_rules() {
        // 不限速字符串整段一帧，显式空分片不产生 token。
        assert_eq!(tokens(Body::Plain("héllo"), 0.0), ["héllo"]);
        let chunks = Text::Chunks(vec!["a".into(), String::new(), "bé".into()]);
        assert_eq!(tokens(Body::Text(&chunks), 0.0), ["a", "bé"]);
        // 限速时按 Unicode scalar 分片，不切断多字节字符。
        assert_eq!(tokens(Body::Plain("aé"), 1_000.0), ["a", "é"]);
        let text = Text::String("xy".into());
        assert_eq!(tokens(Body::Text(&text), 1_000.0), ["x", "y"]);
    }

    #[test]
    fn mock_sse_throttle_stops_when_cancelled() {
        let mut throttle = Throttle::new(0.1).unwrap();
        let shutdown = CancellationToken::new();
        let run_cancel = CancellationToken::new();
        assert!(throttle.ready(&shutdown, &run_cancel).unwrap());
        run_cancel.cancel();
        assert!(!throttle.ready(&shutdown, &run_cancel).unwrap());
        assert!(
            !Throttle::new(0.0)
                .unwrap()
                .ready(&shutdown, &run_cancel)
                .unwrap()
        );
    }

    #[test]
    fn mock_sse_echo_uses_compact_json() {
        assert!(echo_text(None).unwrap().is_none());
        assert!(echo_text(Some(&Value::Null)).unwrap().is_none());
        assert_eq!(
            echo_text(Some(&Value::String("verbatim".into())))
                .unwrap()
                .as_deref(),
            Some("verbatim")
        );
        assert_eq!(
            echo_text(Some(&json!({"echo": true}))).unwrap().as_deref(),
            Some("{\"echo\":true}")
        );
    }
}
