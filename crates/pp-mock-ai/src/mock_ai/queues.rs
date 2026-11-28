use std::{
    collections::BTreeMap,
    io::Cursor,
    ops::Range,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use anyhow::{Result, ensure};
use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{StatusCode, header},
    response::Response,
};
use futures_util::Stream;
use pp_core::cancellation::CancellationToken;
use serde_json::json;
use tokio::sync::mpsc;

use super::{
    MockEndpoint,
    protocol::{self, Instruction},
    sse,
};

type Key = (String, String);
type QueueMap = BTreeMap<Key, Arc<Mutex<Queue>>>;

#[derive(Default)]
pub(super) struct Queues(Mutex<QueueMap>);

#[derive(Default)]
struct Queue {
    script: Arc<Vec<Instruction>>,
    cursor: usize,
    generation: u64,
    busy: bool,
    cancellation: CancellationToken,
}

struct Reservation {
    script: Arc<Vec<Instruction>>,
    steps: Range<usize>,
    generation: u64,
    cancellation: CancellationToken,
}

struct BusyGuard(Arc<Mutex<Queue>>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).busy = false;
    }
}

impl Queues {
    pub(super) fn clear(&self) {
        let mut queues = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for queue in queues.values() {
            queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .cancellation
                .cancel();
        }
        queues.clear();
    }

    pub(super) fn cancel_run(&self, run_id: &str) {
        let mut queues = self.0.lock().unwrap_or_else(|e| e.into_inner());
        queues.retain(|(run, _), queue| {
            if run == run_id {
                queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .cancellation
                    .cancel();
                false
            } else {
                true
            }
        });
    }

    pub(super) fn assert_exhausted(
        &self,
        run_id: &str,
        expected_lanes: Option<usize>,
    ) -> Result<()> {
        let queues = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut found = 0;
        for ((run, _), queue) in queues.iter() {
            if run == run_id {
                found += 1;
                let queue = queue.lock().unwrap_or_else(|e| e.into_inner());
                ensure!(
                    queue.cursor == queue.script.len(),
                    "mock script was not exhausted"
                );
            }
        }
        ensure!(found > 0, "mock run did not request a script");
        ensure!(
            expected_lanes.is_none_or(|expected| found == expected),
            "mock run did not consume every scripted lane"
        );
        Ok(())
    }
}

/// Single fallback entry point: routing, method and queue policy all derive from the raw request-target.
pub(super) async fn handle(State(endpoint): State<MockEndpoint>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let target = parts
        .uri
        .path_and_query()
        .map_or_else(String::new, |target| target.as_str().to_owned());
    let Some((run, lane)) = route(&target) else {
        return empty(StatusCode::NOT_FOUND);
    };
    if parts.method != axum::http::Method::POST {
        return empty(StatusCode::METHOD_NOT_ALLOWED);
    }
    if endpoint.shutdown.is_cancelled() {
        return empty(StatusCode::SERVICE_UNAVAILABLE);
    }
    let queue = {
        let mut queues = endpoint.queues.0.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(queues.entry((run.clone(), lane.clone())).or_default())
    };
    let needs_script = {
        let mut state = queue.lock().unwrap_or_else(|e| e.into_inner());
        if state.busy {
            drop(state);
            return error(
                StatusCode::CONFLICT,
                "mock_queue_busy",
                "Mock queue already has an active response.",
            );
        }
        state.busy = true;
        state.cursor == state.script.len()
    };
    let busy = BusyGuard(Arc::clone(&queue));
    if needs_script {
        // Only a queue that is not yet exhausted reads and parses the body; other requests finish right after the busy check.
        let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_mock_script",
                "mock script request could not be read",
            );
        };
        let parsing =
            tokio::task::spawn_blocking(move || protocol::parse_script(&mut Cursor::new(bytes)));
        let parsed = tokio::select! {
            result = parsing => result,
            _ = endpoint.shutdown.cancelled() => {
                return empty(StatusCode::SERVICE_UNAVAILABLE);
            }
        };
        if endpoint.shutdown.is_cancelled() {
            return empty(StatusCode::SERVICE_UNAVAILABLE);
        }
        let script = match parsed {
            Ok(Ok(script)) => script,
            Ok(Err(failure)) => {
                return error(StatusCode::BAD_REQUEST, failure.code, failure.message);
            }
            Err(join) => {
                // Internal failure: the diagnosis contains no script or credentials and leaves cursor and generation untouched.
                tracing::error!(event = "mock_ai_script_parse_failed", error = %join);
                return empty(StatusCode::INTERNAL_SERVER_ERROR);
            }
        };
        let mut state = queue.lock().unwrap_or_else(|e| e.into_inner());
        let Some(generation) = state.generation.checked_add(1) else {
            drop(state);
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_mock_script",
                "Mock generation limit reached.",
            );
        };
        state.script = Arc::new(script);
        state.cursor = 0;
        state.generation = generation;
    }
    let reservation = {
        let mut state = queue.lock().unwrap_or_else(|e| e.into_inner());
        let start = state.cursor;
        let mut end = start + 1;
        while end < state.script.len() && state.script[end - 1].auto_continue {
            end += 1;
        }
        state.cursor = end;
        Reservation {
            script: Arc::clone(&state.script),
            steps: start..end,
            generation: state.generation,
            cancellation: state.cancellation.clone(),
        }
    };
    let identity = format!(
        "{run}-{lane}-{}-{}",
        reservation.generation, reservation.steps.start
    );
    // A failure instruction occupies one step only: finish with the provider error directly so the client starts its automatic retry.
    if let Some(status) = reservation.script[reservation.steps.start].status {
        return error(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            "mock_http_error",
            "Mock provider failure injected by the fixture.",
        );
    }
    let (sender, receiver) = mpsc::channel(1);
    // The producer only holds the reserved script Arc and range; it never copies the whole script and never holds the queue lock.
    let script = Arc::clone(&reservation.script);
    let steps = reservation.steps.clone();
    let shutdown = endpoint.shutdown.clone();
    let run_cancel = reservation.cancellation.clone();
    tokio::spawn(async move {
        let _ = sse::send(
            sender,
            &script[steps],
            &identity,
            &lane,
            &shutdown,
            &run_cancel,
        )
        .await;
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(BodyStream {
            receiver,
            _busy: busy,
        }))
        .expect("static mock AI SSE response")
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = json!({"error": {
        "type": "invalid_request_error", "code": code, "message": message
    }})
    .to_string();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("static mock AI error response")
}

fn empty(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("static mock AI response")
}

/// Response body: the queue's busy flag is released only at EOF or when the body is dropped.
struct BodyStream {
    receiver: mpsc::Receiver<Bytes>,
    _busy: BusyGuard,
}

impl Stream for BodyStream {
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context).map(|frame| frame.map(Ok))
    }
}

fn route(path: &str) -> Option<(String, String)> {
    if path == "/v1/responses" {
        return Some(("manual".into(), "root".into()));
    }
    let (run, rest) = path.strip_prefix("/mock/")?.split_once('/')?;
    let lane = rest.strip_suffix("/v1/responses")?;
    (valid_identifier(run) && valid_identifier(lane)).then(|| (run.into(), lane.into()))
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}
