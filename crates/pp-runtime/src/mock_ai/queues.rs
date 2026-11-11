use std::{
    collections::BTreeMap,
    ops::Range,
    sync::{Arc, Mutex},
};

use anyhow::{Result, ensure};
use pp_core::cancellation::CancellationToken;
use serde_json::json;
use tiny_http::{Header, Method, Request, Response};

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

pub(super) fn handle(mut request: Request, endpoint: MockEndpoint) {
    let Some((run, lane)) = route(request.url()) else {
        let _ = request.respond(Response::empty(404));
        return;
    };
    if *request.method() != Method::Post {
        let _ = request.respond(Response::empty(405));
        return;
    }
    if endpoint.shutdown.is_cancelled() {
        let _ = request.respond(Response::empty(503));
        return;
    }
    let queue = {
        let mut queues = endpoint.queues.0.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(queues.entry((run.clone(), lane.clone())).or_default())
    };
    let needs_script = {
        let mut state = queue.lock().unwrap_or_else(|e| e.into_inner());
        if state.busy {
            drop(state);
            error(
                request,
                409,
                "mock_queue_busy",
                "Mock queue already has an active response.",
            );
            return;
        }
        state.busy = true;
        state.cursor == state.script.len()
    };
    let _busy = BusyGuard(Arc::clone(&queue));
    if needs_script {
        let script = match protocol::parse_script(request.as_reader()) {
            Ok(script) => script,
            Err(failure) => {
                drop(_busy);
                error(request, 400, failure.code, failure.message);
                return;
            }
        };
        let mut state = queue.lock().unwrap_or_else(|e| e.into_inner());
        let Some(generation) = state.generation.checked_add(1) else {
            drop(state);
            drop(_busy);
            error(
                request,
                400,
                "invalid_mock_script",
                "Mock generation limit reached.",
            );
            return;
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
    // 失败指令只占一个步骤：直接用供应商错误收尾，客户端据此进入自动重试。
    if let Some(status) = reservation.script[reservation.steps.start].status {
        error(
            request,
            status,
            "mock_http_error",
            "Mock provider failure injected by the fixture.",
        );
        return;
    }
    let _ = sse::send(
        request,
        &reservation.script[reservation.steps.clone()],
        &identity,
        &lane,
        &endpoint.shutdown,
        &reservation.cancellation,
    );
}

fn error(request: Request, status: u16, code: &str, message: &str) {
    let response = Response::from_string(
        json!({"error": {
            "type": "invalid_request_error", "code": code, "message": message
        }})
        .to_string(),
    )
    .with_status_code(status)
    .with_header(
        Header::from_bytes("Content-Type", "application/json").expect("static HTTP header"),
    );
    let _ = request.respond(response);
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
