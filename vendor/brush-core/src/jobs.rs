//! Job management

use std::collections::VecDeque;
use std::fmt::Display;
use std::sync::{Arc, Weak};

use futures::FutureExt;

use crate::ExecutionResult;
use crate::PipelineObserver;
use crate::error;
use crate::extensions;
use crate::processes;
use crate::supervisor;
use crate::sys;
use crate::trace_categories;

pub(crate) type JobJoinHandle = tokio::task::JoinHandle<Result<ExecutionResult, error::Error>>;
pub(crate) type JobResult = (Job, Result<ExecutionResult, error::Error>);

/// Manages the jobs that are currently managed by the shell.
#[derive(Default)]
pub struct JobManager {
    /// The jobs that are currently managed by the shell.
    pub jobs: Vec<Job>,
}

/// Represents a task that is part of a job.
pub enum JobTask {
    /// An external process.
    External(processes::ChildProcess),
    /// An internal asynchronous task.
    Internal(JobJoinHandle),
}

/// Represents the result of waiting on a job task.
pub enum JobTaskWaitResult {
    /// The task has completed.
    Completed(ExecutionResult),
    /// The task was stopped.
    Stopped,
}

impl JobTask {
    /// Returns whether the task is an external process.
    pub const fn is_external(&self) -> bool {
        matches!(self, Self::External(_))
    }

    /// Waits for the task to complete. Returns the result of the wait.
    pub async fn wait(&mut self) -> Result<JobTaskWaitResult, error::Error> {
        match self {
            Self::External(process) => {
                let wait_result = process.wait().await?;
                match wait_result {
                    processes::ProcessWaitResult::Completed(output) => {
                        Ok(JobTaskWaitResult::Completed(output.into()))
                    }
                    processes::ProcessWaitResult::Stopped => Ok(JobTaskWaitResult::Stopped),
                }
            }
            Self::Internal(handle) => Ok(JobTaskWaitResult::Completed(handle.await??)),
        }
    }

    /// Polls the task for completion. Returns `Some(result)` if the task has completed,
    /// or `None` if it is still running. The result is the execution result of the task.
    /// Behaves in a best-effort manner; if an internal error occurs during polling,
    /// it will return `None`.
    fn poll(&mut self) -> Option<Result<ExecutionResult, error::Error>> {
        match self {
            Self::External(process) => {
                let check_result = process.poll();
                check_result.map(|polled_result| polled_result.map(|output| output.into()))
            }
            Self::Internal(handle) => {
                let checkable_handle = handle;
                checkable_handle.now_or_never().and_then(|r| r.ok())
            }
        }
    }
}

#[allow(clippy::missing_const_for_fn)]
impl JobManager {
    /// Returns a new job manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a job to the job manager and marks it as the current job;
    /// returns an immutable reference to the job.
    ///
    /// # Arguments
    ///
    /// * `job` - The job to add.
    #[allow(
        clippy::missing_panics_doc,
        reason = "push() guarantees the vector length is >= 1"
    )]
    pub fn add_as_current(&mut self, mut job: Job) -> &Job {
        for j in &mut self.jobs {
            if matches!(j.annotation, JobAnnotation::Current) {
                j.annotation = JobAnnotation::Previous;
                break;
            }
        }

        let id = self.jobs.len() + 1;
        job.id = id;
        job.annotation = JobAnnotation::Current;
        self.jobs.push(job);

        #[allow(clippy::unwrap_used, reason = "we just pushed an element")]
        self.jobs.last().unwrap()
    }

    /// Returns the current job, if there is one.
    pub fn current_job(&self) -> Option<&Job> {
        self.jobs
            .iter()
            .find(|j| matches!(j.annotation, JobAnnotation::Current))
    }

    /// Returns a mutable reference to the current job, if there is one.
    pub fn current_job_mut(&mut self) -> Option<&mut Job> {
        self.jobs
            .iter_mut()
            .find(|j| matches!(j.annotation, JobAnnotation::Current))
    }

    /// Returns the previous job, if there is one.
    pub fn prev_job(&self) -> Option<&Job> {
        self.jobs
            .iter()
            .find(|j| matches!(j.annotation, JobAnnotation::Previous))
    }

    /// Returns a mutable reference to the previous job, if there is one.
    pub fn prev_job_mut(&mut self) -> Option<&mut Job> {
        self.jobs
            .iter_mut()
            .find(|j| matches!(j.annotation, JobAnnotation::Previous))
    }

    /// Tries to resolve the given job specification to a job.
    ///
    /// # Arguments
    ///
    /// * `job_spec` - The job specification to resolve.
    pub fn resolve_job_spec(&mut self, job_spec: &str) -> Option<&mut Job> {
        let index = self.index_for_spec(job_spec)?;
        self.jobs.get_mut(index)
    }

    /// Resolves the given job specification to an owned snapshot of the job.
    ///
    /// Callers that must both signal a job and borrow the shell can use this to
    /// avoid holding two borrows of the same shell at once.
    ///
    /// # Arguments
    ///
    /// * `job_spec` - The job specification to resolve.
    pub fn resolve_job_handle(&self, job_spec: &str) -> Option<JobHandle> {
        let index = self.index_for_spec(job_spec)?;
        let job = self.jobs.get(index)?;

        Some(JobHandle {
            target: job.signal_target(),
            state: job.state.clone(),
            command_line: job.command_line.clone(),
        })
    }

    /// Resolves a job specification to the index of the matching job.
    fn index_for_spec(&self, job_spec: &str) -> Option<usize> {
        let remainder = job_spec.strip_prefix('%')?;

        match remainder {
            "%" | "+" => self
                .jobs
                .iter()
                .position(|job| matches!(job.annotation, JobAnnotation::Current)),
            "-" => self
                .jobs
                .iter()
                .position(|job| matches!(job.annotation, JobAnnotation::Previous)),
            s if s.chars().all(char::is_numeric) => {
                let id = s.parse::<usize>().ok()?;
                self.jobs.iter().position(|job| job.id == id)
            }
            _ => {
                tracing::warn!(target: trace_categories::UNIMPLEMENTED, "unimplemented: job spec naming command: '{job_spec}'");
                None
            }
        }
    }

    /// Returns whether any managed job contains the given process ID.
    ///
    /// # Arguments
    ///
    /// * `pid` - The process ID to look for.
    pub fn contains_pid(&self, pid: sys::process::ProcessId) -> bool {
        self.jobs.iter().any(|job| job.contains_pid(pid))
    }

    /// Finds the job that owns the given process ID, if any.
    ///
    /// # Arguments
    ///
    /// * `pid` - The process ID to look for.
    pub fn find_job_containing_pid(&mut self, pid: sys::process::ProcessId) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|job| job.contains_pid(pid))
    }

    /// Waits for all managed jobs to complete.
    pub async fn wait_all(&mut self) -> Result<Vec<Job>, error::Error> {
        for job in &mut self.jobs {
            job.wait().await?;
        }

        Ok(self.sweep_completed_jobs())
    }

    /// Polls all managed jobs for completion.
    pub fn poll(&mut self) -> Result<Vec<JobResult>, error::Error> {
        let mut results = vec![];

        let mut i = 0;
        while i != self.jobs.len() {
            if let Some(result) = self.jobs[i].poll_done()? {
                let job = self.jobs.remove(i);
                results.push((job, result));
            } else if matches!(self.jobs[i].state, JobState::Done) {
                // TODO(jobs): This is a workaround to remove jobs that are done but for which we
                // don't know what happened.
                results.push((self.jobs.remove(i), Ok(ExecutionResult::success())));
            } else {
                i += 1;
            }
        }

        Ok(results)
    }

    fn sweep_completed_jobs(&mut self) -> Vec<Job> {
        let mut completed_jobs = vec![];

        let mut i = 0;
        while i != self.jobs.len() {
            if self.jobs[i].tasks.is_empty() {
                completed_jobs.push(self.jobs.remove(i));
            } else {
                i += 1;
            }
        }

        completed_jobs
    }
}

/// Represents the current execution state of a job.
#[derive(Clone)]
pub enum JobState {
    /// Unknown state.
    Unknown,
    /// The job is running.
    Running,
    /// The job is stopped.
    Stopped,
    /// The job has completed.
    Done,
}

impl Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => write!(f, "Unknown"),
            Self::Running => write!(f, "Running"),
            Self::Stopped => write!(f, "Stopped"),
            Self::Done => write!(f, "Done"),
        }
    }
}

/// Represents an annotation for a job.
#[derive(Clone)]
pub enum JobAnnotation {
    /// No annotation.
    None,
    /// The job is the current job.
    Current,
    /// The job is the previous job.
    Previous,
}

impl Display for JobAnnotation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, ""),
            Self::Current => write!(f, "+"),
            Self::Previous => write!(f, "-"),
        }
    }
}

/// An owned handle to a job's signalling state.
///
/// Holding this instead of a borrow of the job manager lets a caller signal a
/// group and then re-borrow the shell for unrelated work.
pub struct JobSignalTarget {
    /// Registration lease held by the job, when it is still live.
    managed_group: Option<Arc<dyn supervisor::ManagedGroup>>,
    /// Weak handle retained after the lease is released, when known.
    managed_group_weak: Option<Weak<dyn supervisor::ManagedGroup>>,
    /// Numeric process group ID, when known.
    pgid: Option<sys::process::ProcessId>,
}

#[allow(clippy::missing_const_for_fn)]
impl JobSignalTarget {
    /// Returns the numeric process group ID of the job, when known.
    pub const fn pgid(&self) -> Option<sys::process::ProcessId> {
        self.pgid
    }

    /// Continues a stopped job by sending `SIGCONT` to its process group.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell performing the operation.
    pub fn continue_job<SE: extensions::ShellExtensions>(
        &self,
        shell: &crate::Shell<SE>,
    ) -> Result<(), error::Error> {
        let Some(signum) = sys::signal::continue_signal_number() else {
            return Err(
                error::ErrorKind::NotSupportedOnThisPlatform("continuing a job").into(),
            );
        };

        self.signal(shell, signum)
    }

    /// Sends a raw signal number to the job's process group.
    ///
    /// A managed group is always signalled through its registry handle. The
    /// numeric group ID is never used once the registry entry has gone away,
    /// because the ID may already have been reused.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell performing the operation.
    /// * `signum` - The raw signal number to send.
    pub fn signal<SE: extensions::ShellExtensions>(
        &self,
        shell: &crate::Shell<SE>,
        signum: i32,
    ) -> Result<(), error::Error> {
        if let Some(group) = &self.managed_group {
            return signal_managed_group(shell, group.as_ref(), signum);
        }

        if let Some(weak) = &self.managed_group_weak {
            // The job's lease is gone, so the registry entry may have been
            // reclaimed; never fall back to the bare PGID in that case.
            let Some(group) = weak.upgrade() else {
                return Err(error::ErrorKind::FailedToSendSignal.into());
            };

            return signal_managed_group(shell, group.as_ref(), signum);
        }

        if let Some(pgid) = self.pgid {
            shell.signal_process_group(pgid, signum)
        } else {
            Err(error::ErrorKind::FailedToSendSignal.into())
        }
    }
}

/// Sends a raw signal to a supervised process group through its registry handle.
fn signal_managed_group<SE: extensions::ShellExtensions>(
    shell: &crate::Shell<SE>,
    group: &dyn supervisor::ManagedGroup,
    signum: i32,
) -> Result<(), error::Error> {
    let Some(supervisor) = shell.process_supervisor() else {
        return Err(error::ErrorKind::FailedToSendSignal.into());
    };

    supervisor
        .signal_group(group, signum)
        .map_err(|_err| error::ErrorKind::FailedToSendSignal.into())
}

/// An owned snapshot of a job used to act on it without borrowing the shell.
pub struct JobHandle {
    /// Signal target of the job.
    pub target: JobSignalTarget,
    /// Current state of the job.
    pub state: JobState,
    /// Command line of the job.
    pub command_line: String,
}

/// Reports the stages of a background pipeline as the job reaps them.
pub(crate) struct JobPipelineObservation {
    /// The observer that receives stage and pipeline events.
    observer: Arc<dyn PipelineObserver>,
    /// The pipeline occurrence identifier.
    pipeline: u64,
    /// 1-based stage indexes whose external processes the job owns, in task
    /// order. A stage that did not start a process is not listed.
    pending_stages: VecDeque<usize>,
    /// Whether the occurrence has already been finished.
    finished: bool,
}

impl JobPipelineObservation {
    fn new(observer: Arc<dyn PipelineObserver>, pipeline: u64, stages: Vec<usize>) -> Self {
        Self {
            observer,
            pipeline,
            pending_stages: stages.into(),
            finished: false,
        }
    }

    /// Reports the stage owned by the task removed from the front of the job's
    /// queue.
    fn report_front(&mut self, result: &ExecutionResult) -> Result<(), error::Error> {
        if let Some(stage) = self.pending_stages.pop_front() {
            self.report(stage, result)?;
        }

        Ok(())
    }

    /// Reports the stage owned by the task removed from the back of the job's
    /// queue.
    fn report_back(&mut self, result: &ExecutionResult) -> Result<(), error::Error> {
        if let Some(stage) = self.pending_stages.pop_back() {
            self.report(stage, result)?;
        }

        Ok(())
    }

    fn report(&self, stage: usize, result: &ExecutionResult) -> Result<(), error::Error> {
        if self.finished {
            return Ok(());
        }

        self.observer.stage_done(
            self.pipeline,
            stage,
            u8::from(&result.exit_code),
            result.signal,
        )?;

        Ok(())
    }

    /// Finishes the occurrence once every stage has been reaped.
    fn report_finished(&mut self, status: u8) -> Result<(), error::Error> {
        if self.finished {
            return Ok(());
        }

        self.finished = true;
        self.observer.finish(self.pipeline, status)?;

        Ok(())
    }
}

/// Encapsulates a set of processes managed by the shell as a single unit.
pub struct Job {
    /// The tasks that make up the job.
    tasks: VecDeque<JobTask>,

    /// Observation of the background pipeline that this job directly reaps.
    pipeline_observation: Option<JobPipelineObservation>,

    /// If available, the process group ID of the job's processes.
    pgid: Option<sys::process::ProcessId>,

    /// Registration lease on the supervised process group.
    ///
    /// It is held only while the job is live -- long enough for `kill %n` and
    /// `bg`/`fg` to address the group -- and dropped as soon as the job reaches
    /// a terminal state, so completed jobs never pin a reaped leader.
    managed_group: Option<Arc<dyn supervisor::ManagedGroup>>,

    /// Weak handle to the same group, retained after the lease is released.
    managed_group_weak: Option<Weak<dyn supervisor::ManagedGroup>>,

    /// Registry key of the job's supervised process group, if any.
    managed_group_key: Option<u64>,

    /// The most recent external PID reported for this job.
    ///
    /// It is recorded at construction so that `$!` keeps reporting it after the
    /// job's tasks have been reaped. Logical tasks have no such identity.
    last_background_pid: Option<sys::process::ProcessId>,

    /// The annotation of the job (e.g., current, previous).
    annotation: JobAnnotation,

    /// The shell-internal ID of the job.
    pub id: usize,

    /// The command line of the job.
    pub command_line: String,

    /// The current operational state of the job.
    pub state: JobState,
}

impl Display for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}]{:3}{}\t{}",
            self.id,
            self.annotation.to_string(),
            self.state,
            self.command_line
        )
    }
}

#[allow(clippy::missing_const_for_fn)]
impl Job {
    /// Returns a new job object.
    ///
    /// # Arguments
    ///
    /// * `children` - The job's known child processes.
    /// * `command_line` - The command line of the job.
    /// * `state` - The current operational state of the job.
    pub(crate) fn new<I>(tasks: I, command_line: String, state: JobState) -> Self
    where
        I: IntoIterator<Item = JobTask>,
    {
        let tasks: VecDeque<JobTask> = tasks.into_iter().collect();
        let last_background_pid = tasks.iter().rev().find_map(|task| match task {
            JobTask::External(process) => process.pid(),
            JobTask::Internal(_) => None,
        });

        Self {
            id: 0,
            tasks,
            pipeline_observation: None,
            pgid: None,
            managed_group: None,
            managed_group_weak: None,
            managed_group_key: None,
            last_background_pid,
            annotation: JobAnnotation::None,
            command_line,
            state,
        }
    }

    /// Attaches observation for the background pipeline that this job directly
    /// reaps.
    ///
    /// `stages` lists the 1-based stage indexes whose external processes the job
    /// owns, in task order.
    pub(crate) fn set_pipeline_observation(
        &mut self,
        observer: Arc<dyn PipelineObserver>,
        pipeline: u64,
        stages: Vec<usize>,
    ) {
        self.pipeline_observation = Some(JobPipelineObservation::new(observer, pipeline, stages));
    }

    /// Returns a pid-style string for the job.
    pub fn to_pid_style_string(&self) -> String {
        let display_pid = self
            .representative_pid()
            .map_or_else(|| String::from("<pid unknown>"), |pid| pid.to_string());
        std::format!("[{}]{}\t{}", self.id, self.annotation, display_pid)
    }

    /// Returns the annotation of the job.
    pub fn annotation(&self) -> JobAnnotation {
        self.annotation.clone()
    }

    /// Returns the command name of the job.
    pub fn command_name(&self) -> &str {
        self.command_line
            .split_ascii_whitespace()
            .next()
            .unwrap_or_default()
    }

    /// Returns whether the job is the current job.
    pub const fn is_current(&self) -> bool {
        matches!(self.annotation, JobAnnotation::Current)
    }

    /// Returns whether the job is the previous job.
    pub const fn is_prev(&self) -> bool {
        matches!(self.annotation, JobAnnotation::Previous)
    }

    /// Polls whether the job has completed.
    pub fn poll_done(
        &mut self,
    ) -> Result<Option<Result<ExecutionResult, error::Error>>, error::Error> {
        let mut result: Option<Result<ExecutionResult, error::Error>> = None;

        tracing::debug!(target: trace_categories::JOBS, "Polling job {} for completion...", self.id);

        while !self.tasks.is_empty() {
            let task = &mut self.tasks[0];
            match task.poll() {
                Some(r) => {
                    self.tasks.remove(0);
                    if let Ok(completed) = &r
                        && let Some(observation) = self.pipeline_observation.as_mut()
                    {
                        observation.report_front(completed)?;
                    }
                    result = Some(r);
                }
                None => {
                    return Ok(None);
                }
            }
        }

        tracing::debug!(target: trace_categories::JOBS, "Job {} has completed.", self.id);

        self.state = JobState::Done;
        self.release_group_lease();

        if let Some(Ok(completed)) = &result
            && let Some(observation) = self.pipeline_observation.as_mut()
        {
            observation.report_finished(u8::from(&completed.exit_code))?;
        }

        Ok(result)
    }

    /// Waits for the job to complete.
    ///
    /// The reported status is that of the job's last (rightmost) task, matching
    /// the status a pipeline reports to its caller; the remaining tasks are
    /// still waited for and reaped.
    pub async fn wait(&mut self) -> Result<ExecutionResult, error::Error> {
        let mut last_task_result: Option<ExecutionResult> = None;

        while let Some(task) = self.tasks.back_mut() {
            match task.wait().await? {
                JobTaskWaitResult::Completed(execution_result) => {
                    self.tasks.pop_back();
                    if let Some(observation) = self.pipeline_observation.as_mut() {
                        observation.report_back(&execution_result)?;
                    }

                    if last_task_result.is_none() {
                        last_task_result = Some(execution_result);
                    }
                }
                JobTaskWaitResult::Stopped => {
                    self.state = JobState::Stopped;
                    return Ok(ExecutionResult::stopped());
                }
            }
        }

        self.state = JobState::Done;
        self.release_group_lease();

        let result = last_task_result.unwrap_or_else(ExecutionResult::success);
        if let Some(observation) = self.pipeline_observation.as_mut() {
            observation.report_finished(u8::from(&result.exit_code))?;
        }

        Ok(result)
    }

    /// Adopts the registration lease of the job's supervised process group.
    ///
    /// # Arguments
    ///
    /// * `group` - The pipeline's strong group reference.
    pub(crate) fn set_managed_group(&mut self, group: &processes::ProcessGroupRef) {
        let Some(managed) = group.managed.clone() else {
            return;
        };

        self.managed_group_key = Some(managed.key());
        self.managed_group_weak = Some(Arc::downgrade(&managed));
        self.pgid = Some(group.pgid);
        self.managed_group = Some(managed);
    }

    /// Releases the job's registration lease on its process group.
    ///
    /// Called when the job reaches a terminal state; the key and weak handle are
    /// kept so a stale signal attempt fails instead of addressing a recycled PGID.
    fn release_group_lease(&mut self) {
        self.managed_group = None;
    }

    /// Returns the registry key of the job's supervised process group, if any.
    pub const fn managed_group_key(&self) -> Option<u64> {
        self.managed_group_key
    }

    /// Returns an owned signal target for this job.
    pub fn signal_target(&self) -> JobSignalTarget {
        JobSignalTarget {
            managed_group: self.managed_group.clone(),
            managed_group_weak: self.managed_group_weak.clone(),
            pgid: self.process_group_id(),
        }
    }

    /// Returns whether the job is currently stopped.
    pub const fn is_stopped(&self) -> bool {
        matches!(self.state, JobState::Stopped)
    }

    /// Marks the job as running again.
    pub fn mark_running(&mut self) {
        self.state = JobState::Running;
    }

    /// Returns the process ID of the job's most recently started external process.
    ///
    /// This is the value exposed as `$!`; logical (non-external) tasks have no
    /// operating-system identity and therefore report `None`.
    pub const fn background_pid(&self) -> Option<sys::process::ProcessId> {
        self.last_background_pid
    }

    /// Returns whether the given process ID belongs to one of the job's tasks.
    ///
    /// # Arguments
    ///
    /// * `pid` - The process ID to look for.
    pub fn contains_pid(&self, pid: sys::process::ProcessId) -> bool {
        self.tasks
            .iter()
            .any(|task| matches!(task, JobTask::External(process) if process.pid() == Some(pid)))
    }

    /// Tries to retrieve a "representative" pid for the job.
    pub fn representative_pid(&self) -> Option<sys::process::ProcessId> {
        for task in &self.tasks {
            match task {
                JobTask::External(p) => {
                    if let Some(pid) = p.pid() {
                        return Some(pid);
                    }
                }
                JobTask::Internal(_) => (),
            }
        }
        None
    }

    /// Tries to retrieve the process group ID (PGID) of the job.
    pub fn process_group_id(&self) -> Option<sys::process::ProcessId> {
        // TODO(jobs): Don't assume that the first PID is the PGID.
        self.pgid.or_else(|| self.representative_pid())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Weak};

    use super::*;

    struct FakeGroup {
        pgid: i32,
        key: u64,
    }

    impl supervisor::ManagedGroup for FakeGroup {
        fn pgid(&self) -> i32 {
            self.pgid
        }

        fn key(&self) -> u64 {
            self.key
        }
    }

    fn job_with_group(key: u64) -> (Job, Weak<FakeGroup>) {
        let group = Arc::new(FakeGroup { pgid: 4_321, key });
        let weak = Arc::downgrade(&group);

        let mut job = Job::new(
            std::iter::empty::<JobTask>(),
            "sleep 30".to_owned(),
            JobState::Running,
        );

        job.set_managed_group(&processes::ProcessGroupRef {
            pgid: 4_321,
            managed: Some(group),
        });

        (job, weak)
    }

    #[test]
    fn live_job_retains_a_group_lease_for_signalling() {
        let (job, weak) = job_with_group(7);

        // A background pipeline releases its per-child leases as soon as it is
        // spawned; `kill %1` must still be able to reach the group.
        assert!(weak.upgrade().is_some());
        assert_eq!(job.managed_group_key(), Some(7));

        let target = job.signal_target();
        drop(job);
        assert!(
            weak.upgrade().is_some(),
            "the signal target keeps the lease alive"
        );

        drop(target);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn terminal_job_keeps_only_key_and_weak_handle() {
        let (mut job, weak) = job_with_group(9);

        job.release_group_lease();

        assert!(weak.upgrade().is_none(), "no lease may survive a terminal job");
        assert_eq!(job.managed_group_key(), Some(9));
        assert_eq!(job.signal_target().pgid(), Some(4_321));
    }

    #[tokio::test]
    async fn waiting_a_finished_job_releases_the_group_lease() {
        let (mut job, weak) = job_with_group(11);

        let _ = job.wait().await;

        assert!(weak.upgrade().is_none());
        assert_eq!(job.managed_group_key(), Some(11));
    }
}
