//! Synchronization coordinator for save diagnostics: two managed workers, path-mutation cancellation and a per-owner inbox.
//! Workers never touch the agent, the journal or the terminal; display and model consumption are confirmed separately by the calling thread.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use super::Registry;
use super::diagnostics;
use anyhow::{Result, bail};
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;
use pp_core::cancellation::CleanupError;
use pp_core::lsp_report::DiagnosticReport;
use pp_core::lsp_report::FormatterStatus;
use pp_core::lsp_report::ReportStatus;

const INLINE_WAIT: Duration = Duration::from_millis(500);
const WORKERS: usize = 2;
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
pub struct Coordinator {
    shared: Arc<Shared>,
    started: Mutex<bool>,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Default)]
struct State {
    closed: bool,
    paths: HashMap<PathBuf, Mutation>,
    jobs: VecDeque<Job>,
    errors: VecDeque<anyhow::Error>,
}

struct Mutation {
    epoch: u64,
    cancellation: CancellationToken,
}

/// Must be acquired before waiting on write_lock or a client operation.
#[derive(Clone)]
pub struct Ticket {
    path: PathBuf,
    epoch: u64,
    pub cancellation: CancellationToken,
}

struct Job {
    registry: Registry,
    ticket: Ticket,
    owner: Weak<OwnerInner>,
    generation: u64,
    run: Option<String>,
    origin: String,
    formatter: Option<FormatterStatus>,
    created: bool,
    handoff: Arc<Handoff>,
}

#[derive(Default)]
struct Handoff {
    state: Mutex<HandoffState>,
    ready: Condvar,
}

#[derive(Default)]
struct HandoffState {
    background: bool,
    result: Option<Result<DiagnosticReport>>,
}

pub struct Owner {
    inner: Arc<OwnerInner>,
}

struct OwnerInner {
    id: String,
    state: Mutex<OwnerState>,
}

#[derive(Default)]
struct OwnerState {
    generation: u64,
    next_id: u64,
    run: Option<String>,
    active: HashMap<PathBuf, (u64, CancellationToken)>,
    inbox: VecDeque<Delivery>,
    errors: VecDeque<anyhow::Error>,
    ledger: HashMap<String, HashSet<String>>,
}

/// A frozen report is displayed only once; model consumption cannot be confirmed before the native history commit.
#[derive(Clone)]
pub struct Delivery {
    pub id: u64,
    pub report: DiagnosticReport,
    pub run: Option<String>,
    pub presented: bool,
    pub model_consumed: bool,
    ticket: Ticket,
    generation: u64,
    versions: Vec<(String, i64)>,
}

impl Owner {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(OwnerInner {
                id: format!("lsp-owner-{}", NEXT_OWNER.fetch_add(1, Ordering::Relaxed)),
                state: Mutex::new(OwnerState::default()),
            }),
        }
    }

    pub fn id(&self) -> &str {
        &self.inner.id
    }

    pub fn set_run_id(&self, run: Option<String>) {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .run = run;
    }

    pub fn cancel(&self) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.generation = state.generation.wrapping_add(1);
        for (_, (_, cancellation)) in state.active.drain() {
            cancellation.cancel();
        }
        state.inbox.clear();
        state.errors.clear();
    }

    pub fn reset(&self) {
        self.cancel();
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.ledger.clear();
    }

    /// Return only the reports that still belong to the current owner/session, path mutation and document version.
    pub fn pending(&self, registry: &Registry) -> Result<Vec<Delivery>> {
        if let Some(error) = registry
            .diagnostics()
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .errors
            .pop_front()
        {
            return Err(error);
        }
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(error) = state.errors.pop_front() {
            return Err(error);
        }
        let generation = state.generation;
        state.inbox.retain(|item| {
            item.generation == generation
                && registry.diagnostics().current(&item.ticket)
                && item.versions.iter().all(|(identity, version)| {
                    registry
                        .started()
                        .iter()
                        .find(|client| &client.identity == identity)
                        .and_then(|client| client.document(&item.ticket.path))
                        .is_some_and(|document| document.version == *version)
                })
        });
        Ok(state.inbox.iter().cloned().collect())
    }

    pub fn presented(&self, id: u64) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(item) = state.inbox.iter_mut().find(|item| item.id == id) {
            item.presented = true;
        }
        state
            .inbox
            .retain(|item| !item.presented || !item.model_consumed);
    }

    pub fn consumed(&self, ids: &[u64]) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        for item in &mut state.inbox {
            if ids.contains(&item.id) {
                item.model_consumed = true;
            }
        }
        state
            .inbox
            .retain(|item| !item.presented || !item.model_consumed);
    }

    pub fn deduplicate(&self, report: &mut DiagnosticReport, enabled: bool) {
        deduplicate(
            &mut self.inner.state.lock().unwrap_or_else(|e| e.into_inner()),
            report,
            enabled,
        );
    }
}

impl Default for Owner {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl Coordinator {
    pub fn mutate(&self, path: &Path) -> Ticket {
        let mut removed_handoffs = Vec::new();
        let ticket = {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            let epoch = match state.paths.get(path) {
                Some(previous) => {
                    previous.cancellation.cancel();
                    previous.epoch.wrapping_add(1)
                }
                None => 1,
            };
            let cancellation = CancellationToken::new();
            if state.closed {
                cancellation.cancel();
            }
            state.paths.insert(
                path.to_path_buf(),
                Mutation {
                    epoch,
                    cancellation: cancellation.clone(),
                },
            );
            let mut remaining = VecDeque::with_capacity(state.jobs.len());
            while let Some(job) = state.jobs.pop_front() {
                if job.ticket.path == path {
                    removed_handoffs.push(job.handoff);
                } else {
                    remaining.push_back(job);
                }
            }
            state.jobs = remaining;
            Ticket {
                path: path.to_path_buf(),
                epoch,
                cancellation,
            }
        };
        for handoff in removed_handoffs {
            cancel_handoff(&handoff);
        }
        ticket
    }

    pub(crate) fn current(&self, ticket: &Ticket) -> bool {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        !state.closed
            && !ticket.cancellation.is_cancelled()
            && state
                .paths
                .get(&ticket.path)
                .is_some_and(|current| current.epoch == ticket.epoch)
    }

    pub(crate) fn stop(&self) -> Result<()> {
        let _started = self.started.lock().unwrap_or_else(|e| e.into_inner());
        self.shutdown()
    }

    fn shutdown(&self) -> Result<()> {
        let dropped = {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            for mutation in state.paths.values() {
                mutation.cancellation.cancel();
            }
            let dropped: Vec<_> = state.jobs.drain(..).map(|job| job.handoff).collect();
            self.shared.wake.notify_all();
            dropped
        };
        for handoff in dropped {
            cancel_handoff(&handoff);
        }
        let workers = std::mem::take(&mut *self.workers.lock().unwrap_or_else(|e| e.into_inner()));
        for worker in workers {
            // The last Registry handle may also be dropped by this worker; it cannot join itself.
            if worker.thread().id() != std::thread::current().id() && worker.join().is_err() {
                self.shared
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .errors
                    .push_back(
                        CleanupError(anyhow::anyhow!("LSP diagnostics worker panicked")).into(),
                    );
            }
        }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut error) = state.errors.pop_front() {
            for additional in state.errors.drain(..) {
                error = error.context(format!(
                    "Additional diagnostics cleanup failure: {additional:#}"
                ));
            }
            return Err(error);
        }
        Ok(())
    }

    fn start(&self) -> Result<()> {
        let mut started = self.started.lock().unwrap_or_else(|e| e.into_inner());
        if *started {
            return Ok(());
        }
        for index in 0..WORKERS {
            let shared = self.shared.clone();
            match std::thread::Builder::new()
                .name(format!("pp-lsp-diagnostics-{index}"))
                .spawn(move || worker(shared))
            {
                Ok(worker) => self
                    .workers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(worker),
                Err(error) => {
                    return match self.shutdown() {
                        Ok(()) => Err(error.into()),
                        Err(cleanup) => {
                            Err(cleanup
                                .context(format!("Starting diagnostics worker failed: {error}")))
                        }
                    };
                }
            }
        }
        *started = true;
        Ok(())
    }

    /// The single inline wait budget; after a timeout the same job continues without re-sending the diagnostic request.
    pub fn collect_after_save(
        &self,
        registry: &Registry,
        owner: &Owner,
        ticket: Ticket,
        origin: &str,
        created: bool,
        formatter: Option<FormatterStatus>,
    ) -> Result<Option<DiagnosticReport>> {
        self.start()?;
        let handoff = Arc::new(Handoff::default());
        let (generation, run) = {
            let mut state = owner.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            state.active.retain(|_, (_, token)| !token.is_cancelled());
            state.active.insert(
                ticket.path.clone(),
                (ticket.epoch, ticket.cancellation.clone()),
            );
            (state.generation, state.run.clone())
        };
        let ticket_cancellation = ticket.cancellation.clone();
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed || ticket.cancellation.is_cancelled() {
                bail!(Cancelled);
            }
            state.jobs.push_back(Job {
                registry: registry.clone(),
                ticket,
                owner: Arc::downgrade(&owner.inner),
                generation,
                run,
                origin: origin.to_owned(),
                formatter,
                created,
                handoff: handoff.clone(),
            });
            self.shared.wake.notify_one();
        }
        let deadline = Instant::now() + INLINE_WAIT;
        let mut state = handoff.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(error) = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .errors
                .pop_front()
            {
                return Err(error);
            }
            if let Some(result) = state.result.take() {
                let mut report = result?;
                owner.deduplicate(&mut report, registry.settings().diagnostics_deduplicate);
                return Ok(Some(report));
            }
            if ticket_cancellation.is_cancelled() {
                bail!(Cancelled);
            }
            let now = Instant::now();
            if now >= deadline {
                state.background = true;
                return Ok(None);
            }
            let (next, _) = handoff
                .ready
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            state = next;
        }
    }
}

impl Drop for Coordinator {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::error!(event = "lsp_diagnostics_cleanup", error = %error);
        }
    }
}

fn worker(shared: Arc<Shared>) {
    loop {
        let job = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.closed {
                    return;
                }
                if let Some(job) = state.jobs.pop_front() {
                    break job;
                }
                state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(owner) = job.owner.upgrade() else {
            cancel_handoff(&job.handoff);
            continue;
        };
        if !job.registry.diagnostics().current(&job.ticket) {
            cancel_handoff(&job.handoff);
            continue;
        }
        let wait = job.registry.settings().request_timeout.max(12);
        let deadline = Instant::now() + Duration::from_secs((wait + 10).max(25));
        let sync_warnings = sync_saved(
            &job.registry,
            &job.ticket.path,
            job.created,
            deadline,
            &job.ticket.cancellation,
        );
        let mut result = diagnostics::collect(
            &job.registry,
            &job.ticket.path,
            &job.origin,
            &owner.id,
            true,
            deadline,
            &job.ticket.cancellation,
        );
        if let Ok(report) = &mut result {
            report.formatter = job.formatter;
            if !sync_warnings.is_empty() {
                report.servers.push(pp_core::lsp_report::ServerReport {
                    server: "document synchronization".to_owned(),
                    status: ReportStatus::Failed,
                    note: Some(sync_warnings.join("; ")),
                    diagnostics: Vec::new(),
                });
            }
        }
        {
            let mut state = owner.state.lock().unwrap_or_else(|e| e.into_inner());
            if state
                .active
                .get(&job.ticket.path)
                .is_some_and(|(epoch, _)| *epoch == job.ticket.epoch)
            {
                state.active.remove(&job.ticket.path);
            }
        }
        // Cleanup failures are exempt from the mutation/session expiry drop rules and must still surface on exit.
        if result
            .as_ref()
            .is_err_and(|error| error.is::<CleanupError>())
        {
            if let Err(error) = result {
                shared
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .errors
                    .push_back(error);
            }
            let _handoff = job.handoff.state.lock().unwrap_or_else(|e| e.into_inner());
            job.handoff.ready.notify_one();
            continue;
        }
        if !job.registry.diagnostics().current(&job.ticket) {
            cancel_handoff(&job.handoff);
            continue;
        }
        let mut handoff = job.handoff.state.lock().unwrap_or_else(|e| e.into_inner());
        if !handoff.background {
            handoff.result = Some(result);
            job.handoff.ready.notify_one();
            continue;
        }
        drop(handoff);
        let mut state = owner.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.generation != job.generation || job.ticket.cancellation.is_cancelled() {
            continue;
        }
        match result {
            Ok(mut report) => {
                deduplicate(
                    &mut state,
                    &mut report,
                    job.registry.settings().diagnostics_deduplicate,
                );
                let versions = job
                    .registry
                    .started()
                    .into_iter()
                    .filter_map(|client| {
                        client
                            .document(&job.ticket.path)
                            .map(|document| (client.identity.clone(), document.version))
                    })
                    .collect();
                state.next_id = state.next_id.wrapping_add(1);
                let id = state.next_id;
                state.inbox.push_back(Delivery {
                    id,
                    report,
                    run: job.run,
                    presented: false,
                    model_consumed: false,
                    ticket: job.ticket,
                    generation: job.generation,
                    versions,
                });
            }
            Err(error) if error.is::<Cancelled>() => {}
            Err(error) if error.is::<CleanupError>() => state.errors.push_back(error),
            Err(error) => state.errors.push_back(error),
        }
    }
}

fn deduplicate(state: &mut OwnerState, report: &mut DiagnosticReport, enabled: bool) {
    // Unverified/partial failures must not clear previous problems; only valid reports take part in the ledger.
    let verified = report.servers.iter().any(|server| {
        matches!(
            server.status,
            ReportStatus::ReportedClean | ReportStatus::ReportedIssues
        )
    });
    if !verified {
        return;
    }
    let mut current: HashSet<String> = report
        .servers
        .iter()
        .flat_map(|server| &server.diagnostics)
        .map(|diagnostic| message_identity(&diagnostic.message).to_owned())
        .collect();
    let complete = report.servers.iter().all(|server| {
        !matches!(
            server.status,
            ReportStatus::Failed | ReportStatus::Unreported
        )
    });
    let previous = state.ledger.remove(&report.path);
    if !complete && let Some(previous) = &previous {
        current.extend(previous.iter().cloned());
    }
    state.ledger.insert(report.path.clone(), current);
    if enabled && let Some(previous) = previous {
        for server in &mut report.servers {
            server.diagnostics.retain(|diagnostic| {
                let keep = !previous.contains(message_identity(&diagnostic.message));
                if !keep {
                    report.deduplicated += 1;
                }
                keep
            });
        }
    }
    let mut remaining = diagnostics::AUTO_MESSAGE_LIMIT;
    for server in &mut report.servers {
        let retained = remaining.min(server.diagnostics.len());
        report.omitted += server.diagnostics.len() - retained;
        server.diagnostics.truncate(retained);
        remaining -= retained;
    }
}

fn message_identity(message: &str) -> &str {
    // CLI diagnostics may carry their own `path:line:column: ` prefix; a changed position does not mean a new problem.
    if let Some((prefix, suffix)) = message.split_once(": ") {
        let mut positions = prefix.rsplit(':');
        if positions
            .next()
            .is_some_and(|value| value.parse::<u32>().is_ok())
            && positions
                .next()
                .is_some_and(|value| value.parse::<u32>().is_ok())
        {
            return suffix.trim_start();
        }
    }
    message
}

pub fn sync_saved(
    registry: &Registry,
    path: &Path,
    created: bool,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Vec<String> {
    let mut warnings = super::edits::sync_file_events(
        registry,
        &[(path.to_path_buf(), if created { 1 } else { 2 })],
        deadline,
        cancellation,
    );
    if created {
        for client in registry
            .started()
            .into_iter()
            .filter(|client| registry.covers(client, path))
        {
            let mut deny = |request: super::transport::ServerRequest| {
                request.respond(
                serde_json::json!({"applied":false,"failureReason":super::transport::UNAUTHORIZED_EDIT}))
            };
            if let Err(error) = client.operation(deadline, cancellation, &mut deny, |op| {
                op.notify(
                    "workspace/didCreateFiles",
                    serde_json::json!({
                        "files": [{"uri": super::uri::path_to_uri(path)}]
                    }),
                )
            }) {
                warnings.push(format!(
                    "Failed to notify {} of created file: {error}",
                    client.name
                ));
            }
        }
    }
    warnings
}

fn cancel_handoff(handoff: &Handoff) {
    let mut state = handoff.state.lock().unwrap_or_else(|e| e.into_inner());
    if !state.background && state.result.is_none() {
        state.result = Some(Err(Cancelled.into()));
        handoff.ready.notify_one();
    }
}

#[cfg(test)]
mod tests;
