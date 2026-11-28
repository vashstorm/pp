//! SSE encoding for the Responses API: single lifecycle, native item events and cancellable token pacing.
//!
//! This module only produces SSE frame bytes; the HTTP status line, chunked framing and connection are handled by Axum/Hyper.

use std::{
    borrow::Cow,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use axum::body::Bytes;
use pp_core::cancellation::CancellationToken;
use serde_json::{Value, json};
use tokio::{
    sync::mpsc,
    time::{Instant, sleep_until},
};

use super::protocol::{self, Instruction, Text, Tool};

/// Encode one already-reserved instruction group into Responses SSE frames and write them to the capacity-1 body channel.
///
/// The queue's busy flag and cancellation are owned by the caller; this only checks cancellation
/// and receiver closure between chunks and during rate waits, and stops as soon as a write fails.
pub(super) async fn send(
    sender: mpsc::Sender<Bytes>,
    instructions: &[Instruction],
    identity: &str,
    lane: &str,
    shutdown: &CancellationToken,
    run_cancel: &CancellationToken,
) -> Result<()> {
    let usage = protocol::group_usage(instructions)?;
    let mut stream = Stream::new(sender);
    if stopped(shutdown, run_cancel, &stream.sender) {
        return Ok(());
    }
    let id = format!("resp-{identity}");
    let model = format!("pp-mock-{lane}");
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    if !stream
        .event(
            "response.created",
            json!({"response": response_body(&id, &model, created_at, "in_progress", &[])}),
            shutdown,
            run_cancel,
        )
        .await?
    {
        return Ok(());
    }
    if !stream
        .event(
            "response.in_progress",
            json!({"response": response_body(&id, &model, created_at, "in_progress", &[])}),
            shutdown,
            run_cancel,
        )
        .await?
    {
        return Ok(());
    }
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
            return Ok(());
        }
        let step = u64::try_from(step).context("mock AI script is too long")?;
        let mut throttle = Throttle::new(instruction.tokens_per_second)?;
        let think = full_text(&instruction.think);
        if !think.is_empty()
            && !turn
                .reasoning(step, &think, &instruction.think, &mut throttle)
                .await?
        {
            return Ok(());
        }
        let content = full_text(&instruction.content);
        if !content.is_empty()
            && !turn
                .message(
                    "msg",
                    step,
                    &content,
                    Body::Text(&instruction.content),
                    &mut throttle,
                )
                .await?
        {
            return Ok(());
        }
        if let Some(echo) = echo_text(instruction.response.as_ref())? {
            let echo = echo.as_ref();
            if !turn
                .message("echo", step, echo, Body::Plain(echo), &mut throttle)
                .await?
            {
                return Ok(());
            }
        }
        if let Some(tool) = instruction.tool.as_ref()
            && !turn.function_call(step, tool, &mut throttle).await?
        {
            return Ok(());
        }
    }
    if !turn.running() {
        return Ok(());
    }
    let mut response = response_body(&id, &model, created_at, "completed", &turn.output);
    response["usage"] = usage.unwrap_or(Value::Null);
    turn.stream
        .event(
            "response.completed",
            json!({"response": response}),
            shutdown,
            run_cancel,
        )
        .await?;
    Ok(())
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

/// Full message text: a string is borrowed directly, explicit chunks are joined once only when the full text is needed.
fn full_text(text: &Text) -> Cow<'_, str> {
    match text {
        Text::String(text) => Cow::Borrowed(text),
        Text::Chunks(_) => Cow::Owned(text.joined()),
    }
}

/// `response` is pure data echo: strings are emitted verbatim, other non-null values use compact JSON.
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
        !stopped(self.shutdown, self.run_cancel, &self.stream.sender)
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

    async fn reasoning(
        &mut self,
        step: u64,
        text: &str,
        body: &Text,
        throttle: &mut Throttle,
    ) -> Result<bool> {
        let item_id = self.item_id("reasoning", step);
        let index = self.take_index()?;
        if !self
            .stream
            .event(
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
                self.shutdown,
                self.run_cancel,
            )
            .await?
        {
            return Ok(false);
        }
        if !self
            .stream
            .event(
                "response.reasoning_summary_part.added",
                json!({
                    "item_id": item_id,
                    "output_index": index,
                    "summary_index": 0,
                    "part": {"type": "summary_text", "text": ""}
                }),
                self.shutdown,
                self.run_cancel,
            )
            .await?
        {
            return Ok(false);
        }
        let emit = |delta: &str| {
            json!({
                "item_id": item_id,
                "output_index": index,
                "summary_index": 0,
                "delta": delta
            })
        };
        if !emit_body(
            &mut self.stream,
            throttle,
            Body::Text(body),
            self.shutdown,
            self.run_cancel,
            "response.reasoning_summary_text.delta",
            emit,
        )
        .await?
        {
            return Ok(false);
        }
        if !self.running() {
            return Ok(false);
        }
        if !self
            .stream
            .event(
                "response.reasoning_summary_text.done",
                json!({
                    "item_id": item_id,
                    "output_index": index,
                    "summary_index": 0,
                    "text": text
                }),
                self.shutdown,
                self.run_cancel,
            )
            .await?
        {
            return Ok(false);
        }
        let part = json!({"type": "summary_text", "text": text});
        if !self
            .stream
            .event(
                "response.reasoning_summary_part.done",
                json!({
                    "item_id": item_id,
                    "output_index": index,
                    "summary_index": 0,
                    "part": part
                }),
                self.shutdown,
                self.run_cancel,
            )
            .await?
        {
            return Ok(false);
        }
        let item = json!({
            "type": "reasoning",
            "id": item_id,
            "summary": [part],
            "status": "completed"
        });
        if !self
            .stream
            .event(
                "response.output_item.done",
                json!({"output_index": index, "item": item}),
                self.shutdown,
                self.run_cancel,
            )
            .await?
        {
            return Ok(false);
        }
        self.output.push(item);
        Ok(self.running())
    }

    async fn message(
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
        if !self
            .stream
            .event(
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
                self.shutdown,
                self.run_cancel,
            )
            .await?
        {
            return Ok(false);
        }
        if !self
            .stream
            .event(
                "response.content_part.added",
                json!({
                    "item_id": item_id,
                    "output_index": index,
                    "content_index": 0,
                    "part": output_text("")
                }),
                self.shutdown,
                self.run_cancel,
            )
            .await?
        {
            return Ok(false);
        }
        let emit = |delta: &str| {
            json!({
                "item_id": item_id,
                "output_index": index,
                "content_index": 0,
                "delta": delta
            })
        };
        if !emit_body(
            &mut self.stream,
            throttle,
            body,
            self.shutdown,
            self.run_cancel,
            "response.output_text.delta",
            emit,
        )
        .await?
        {
            return Ok(false);
        }
        if !self.running() {
            return Ok(false);
        }
        if !self
            .stream
            .event(
                "response.output_text.done",
                json!({
                    "item_id": item_id,
                    "output_index": index,
                    "content_index": 0,
                    "text": text,
                    "logprobs": null
                }),
                self.shutdown,
                self.run_cancel,
            )
            .await?
        {
            return Ok(false);
        }
        if !self
            .stream
            .event(
                "response.content_part.done",
                json!({
                    "item_id": item_id,
                    "output_index": index,
                    "content_index": 0,
                    "part": output_text(text)
                }),
                self.shutdown,
                self.run_cancel,
            )
            .await?
        {
            return Ok(false);
        }
        let item = json!({
            "type": "message",
            "id": item_id,
            "role": "assistant",
            "status": "completed",
            "content": [output_text(text)]
        });
        if !self
            .stream
            .event(
                "response.output_item.done",
                json!({"output_index": index, "item": item}),
                self.shutdown,
                self.run_cancel,
            )
            .await?
        {
            return Ok(false);
        }
        self.output.push(item);
        Ok(self.running())
    }

    async fn function_call(
        &mut self,
        step: u64,
        tool: &Tool,
        throttle: &mut Throttle,
    ) -> Result<bool> {
        let arguments =
            serde_json::to_string(&tool.arguments).context("serializing mock tool arguments")?;
        let item_id = self.item_id("fc", step);
        let call_id = format!("call-{}-{step}", self.identity);
        let index = self.take_index()?;
        if !self
            .stream
            .event(
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
                self.shutdown,
                self.run_cancel,
            )
            .await?
        {
            return Ok(false);
        }
        let emit = |delta: &str| {
            json!({
                "item_id": item_id,
                "output_index": index,
                "delta": delta
            })
        };
        if !emit_body(
            &mut self.stream,
            throttle,
            Body::Plain(&arguments),
            self.shutdown,
            self.run_cancel,
            "response.function_call_arguments.delta",
            emit,
        )
        .await?
        {
            return Ok(false);
        }
        if !self.running() {
            return Ok(false);
        }
        if !self
            .stream
            .event(
                "response.function_call_arguments.done",
                json!({
                    "item_id": item_id,
                    "output_index": index,
                    "name": tool.name,
                    "arguments": arguments
                }),
                self.shutdown,
                self.run_cancel,
            )
            .await?
        {
            return Ok(false);
        }
        let item = json!({
            "type": "function_call",
            "id": item_id,
            "call_id": call_id,
            "name": tool.name,
            "arguments": arguments,
            "status": "completed"
        });
        if !self
            .stream
            .event(
                "response.output_item.done",
                json!({"output_index": index, "item": item}),
                self.shutdown,
                self.run_cancel,
            )
            .await?
        {
            return Ok(false);
        }
        self.output.push(item);
        Ok(self.running())
    }
}

fn output_text(text: &str) -> Value {
    json!({"type": "output_text", "text": text, "annotations": [], "logprobs": null})
}

/// Message source: explicit chunks or a plain string (echo, tool arguments).
enum Body<'a> {
    Text(&'a Text),
    Plain(&'a str),
}

/// Write one mock token at a time; `false` means cancellation or receiver closure happened before the write.
async fn emit_body(
    stream: &mut Stream,
    throttle: &mut Throttle,
    body: Body<'_>,
    shutdown: &CancellationToken,
    run_cancel: &CancellationToken,
    kind: &str,
    emit: impl Fn(&str) -> Value,
) -> Result<bool> {
    match body {
        Body::Text(Text::Chunks(chunks)) => {
            for chunk in chunks {
                // An explicit empty element is not a mock token and produces no empty frame.
                if chunk.is_empty() {
                    continue;
                }
                if !throttle.ready(shutdown, run_cancel, &stream.sender).await? {
                    return Ok(false);
                }
                if !stream
                    .event(kind, emit(chunk), shutdown, run_cancel)
                    .await?
                {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Body::Text(Text::String(text)) => {
            emit_plain(stream, throttle, text, shutdown, run_cancel, kind, &emit).await
        }
        Body::Plain(text) => {
            emit_plain(stream, throttle, text, shutdown, run_cancel, kind, &emit).await
        }
    }
}

/// An unthrottled string is a single frame; when throttled it is split by Unicode scalar, never by UTF-8 byte.
async fn emit_plain(
    stream: &mut Stream,
    throttle: &mut Throttle,
    text: &str,
    shutdown: &CancellationToken,
    run_cancel: &CancellationToken,
    kind: &str,
    emit: &impl Fn(&str) -> Value,
) -> Result<bool> {
    if text.is_empty() {
        return Ok(true);
    }
    if !throttle.limited() {
        if !throttle.ready(shutdown, run_cancel, &stream.sender).await? {
            return Ok(false);
        }
        return stream.event(kind, emit(text), shutdown, run_cancel).await;
    }
    let mut buffer = [0; 4];
    for character in text.chars() {
        if !throttle.ready(shutdown, run_cancel, &stream.sender).await? {
            return Ok(false);
        }
        if !stream
            .event(
                kind,
                emit(character.encode_utf8(&mut buffer)),
                shutdown,
                run_cancel,
            )
            .await?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn stopped(
    shutdown: &CancellationToken,
    run_cancel: &CancellationToken,
    sender: &mpsc::Sender<Bytes>,
) -> bool {
    shutdown.is_cancelled() || run_cancel.is_cancelled() || sender.is_closed()
}

/// Return once any stop signal arrives: global shutdown, run cancellation or body receiver closure.
async fn halt(
    shutdown: &CancellationToken,
    run_cancel: &CancellationToken,
    sender: &mpsc::Sender<Bytes>,
) {
    tokio::select! {
        _ = shutdown.cancelled() => {}
        _ = run_cancel.cancelled() => {}
        _ = sender.closed() => {}
    }
}

/// Monotonic-clock pacing per instruction; the first token is sent immediately.
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
        anyhow::ensure!(
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

    /// Wait for the deadline of the next mock token; `false` means cancellation or receiver closure while waiting.
    async fn ready(
        &mut self,
        shutdown: &CancellationToken,
        run_cancel: &CancellationToken,
        sender: &mpsc::Sender<Bytes>,
    ) -> Result<bool> {
        if stopped(shutdown, run_cancel, sender) {
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
        tokio::select! {
            _ = sleep_until(deadline) => {}
            _ = halt(shutdown, run_cancel, sender) => return Ok(false),
        }
        self.next = Some(
            deadline
                .checked_add(interval)
                .context("mock AI stream deadline overflow")?,
        );
        Ok(true)
    }
}

/// SSE frame encoding with an incrementing `sequence_number`; writes frames only, never HTTP framing.
struct Stream {
    sender: mpsc::Sender<Bytes>,
    sequence: u64,
}

impl Stream {
    fn new(sender: mpsc::Sender<Bytes>) -> Self {
        Self {
            sender,
            sequence: 0,
        }
    }

    /// Send one frame; `false` means cancellation or receiver closure.
    async fn event(
        &mut self,
        kind: &str,
        mut data: Value,
        shutdown: &CancellationToken,
        run_cancel: &CancellationToken,
    ) -> Result<bool> {
        let object = data
            .as_object_mut()
            .context("mock AI SSE event payload must be an object")?;
        object.insert("type".to_owned(), Value::String(kind.to_owned()));
        object.insert("sequence_number".to_owned(), Value::from(self.sequence));
        self.sequence = self.sequence.saturating_add(1);
        let frame = Bytes::from(format!("event: {kind}\ndata: {data}\n\n"));
        if stopped(shutdown, run_cancel, &self.sender) {
            return Ok(false);
        }
        tokio::select! {
            result = self.sender.send(frame) => Ok(result.is_ok()),
            _ = halt(shutdown, run_cancel, &self.sender) => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect the `type` and `sequence_number` of each frame, plus its `data` payload.
    async fn collect(receiver: &mut mpsc::Receiver<Bytes>) -> Vec<(String, Value)> {
        let mut frames = Vec::new();
        while let Some(frame) = receiver.recv().await {
            let frame = String::from_utf8(frame.to_vec()).unwrap();
            let (kind, data) = frame
                .strip_prefix("event: ")
                .and_then(|frame| frame.split_once("\ndata: "))
                .and_then(|(kind, data)| data.strip_suffix("\n\n").map(|data| (kind, data)))
                .expect("SSE frame must be event + data");
            let data: Value = serde_json::from_str(data).unwrap();
            assert_eq!(data["type"], kind);
            frames.push((kind.to_owned(), data));
        }
        frames
    }

    #[tokio::test]
    async fn mock_sse_frames_are_sequenced() {
        let (sender, mut receiver) = mpsc::channel(2);
        let shutdown = CancellationToken::new();
        let run_cancel = CancellationToken::new();
        let mut stream = Stream::new(sender);
        assert!(
            stream
                .event(
                    "response.created",
                    json!({"response": {"id": "resp"}}),
                    &shutdown,
                    &run_cancel,
                )
                .await
                .unwrap()
        );
        assert!(
            stream
                .event(
                    "response.in_progress",
                    json!({"response": {"id": "resp"}}),
                    &shutdown,
                    &run_cancel,
                )
                .await
                .unwrap()
        );
        drop(stream);
        let frames = collect(&mut receiver).await;
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].0, "response.created");
        assert_eq!(frames[0].1["sequence_number"], 0);
        assert_eq!(frames[1].0, "response.in_progress");
        assert_eq!(frames[1].1["sequence_number"], 1);
    }

    /// Collect `delta` frame by frame: the channel capacity holds these frames, so no consumer task is needed.
    async fn tokens(body: Body<'_>, rate: f64) -> Vec<String> {
        let (sender, mut receiver) = mpsc::channel(16);
        let shutdown = CancellationToken::new();
        let run_cancel = CancellationToken::new();
        let mut stream = Stream::new(sender);
        let mut throttle = Throttle::new(rate).unwrap();
        let emit = |delta: &str| json!({"delta": delta});
        assert!(
            emit_body(
                &mut stream,
                &mut throttle,
                body,
                &shutdown,
                &run_cancel,
                "delta",
                emit,
            )
            .await
            .unwrap()
        );
        drop(stream);
        let mut collected = Vec::new();
        while let Some(frame) = receiver.recv().await {
            let frame = String::from_utf8(frame.to_vec()).unwrap();
            let data = frame
                .strip_prefix("event: delta\ndata: ")
                .and_then(|data| data.strip_suffix("\n\n"))
                .expect("delta frame");
            let data: Value = serde_json::from_str(data).unwrap();
            collected.push(data["delta"].as_str().unwrap().to_owned());
        }
        collected
    }

    #[tokio::test]
    async fn mock_sse_tokens_follow_rate_and_chunk_rules() {
        // An unthrottled string is one frame and explicit empty chunks produce no token.
        assert_eq!(tokens(Body::Plain("héllo"), 0.0).await, ["héllo"]);
        let chunks = Text::Chunks(vec!["a".into(), String::new(), "bé".into()]);
        assert_eq!(tokens(Body::Text(&chunks), 0.0).await, ["a", "bé"]);
        // When throttled, text is split by Unicode scalar and multi-byte characters stay intact.
        assert_eq!(tokens(Body::Plain("aé"), 1_000.0).await, ["a", "é"]);
        let text = Text::String("xy".into());
        assert_eq!(tokens(Body::Text(&text), 1_000.0).await, ["x", "y"]);
    }

    #[tokio::test]
    async fn mock_sse_throttle_stops_when_cancelled() {
        let (sender, receiver) = mpsc::channel(1);
        let shutdown = CancellationToken::new();
        let run_cancel = CancellationToken::new();
        let mut throttle = Throttle::new(0.1).unwrap();
        assert!(
            throttle
                .ready(&shutdown, &run_cancel, &sender)
                .await
                .unwrap()
        );
        run_cancel.cancel();
        assert!(
            !throttle
                .ready(&shutdown, &run_cancel, &sender)
                .await
                .unwrap()
        );
        assert!(
            !Throttle::new(0.0)
                .unwrap()
                .ready(&shutdown, &run_cancel, &sender)
                .await
                .unwrap()
        );
        drop(receiver);
    }

    #[tokio::test]
    async fn mock_sse_stops_when_the_body_is_dropped() {
        let (sender, receiver) = mpsc::channel(1);
        let shutdown = CancellationToken::new();
        let run_cancel = CancellationToken::new();
        let mut stream = Stream::new(sender);
        drop(receiver);
        assert!(
            !stream
                .event(
                    "response.created",
                    json!({"response": {"id": "resp"}}),
                    &shutdown,
                    &run_cancel,
                )
                .await
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
