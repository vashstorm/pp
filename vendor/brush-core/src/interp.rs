use brush_parser::ast::{self, CommandPrefixOrSuffixItem, SourceLocation};
use itertools::Itertools;
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::arithmetic::{self, ExpandAndEvaluate};
use crate::commands::{self, CommandArg};
use crate::env::{EnvironmentLookup, EnvironmentScope, valid_variable_name};
use crate::openfiles::{OpenFile, OpenFiles};
use crate::pipeline::PipelineObserver;
use crate::processes::ProcessGroupRef;
use crate::results::{
    ExecutionExitCode, ExecutionResult, ExecutionSpawnResult, ExecutionWaitResult,
};
use crate::shell::{RedirectCreation, Shell};
use crate::supervisor::{CleanupPermit, ProcessSupervisor};
use crate::variables::{
    ArrayLiteral, ShellValue, ShellValueLiteral, ShellValueUnsetType, ShellVariable,
};
use crate::{
    ShellFd, error, expansion, extendedtests, extensions, ioutils, jobs, openfiles, sys, timing,
};

/// Encapsulates the context of execution in a command pipeline.
struct PipelineExecutionContext<'a, SE: extensions::ShellExtensions> {
    /// The shell in which the command should be executed.
    shell: commands::ShellForCommand<'a, SE>,
    /// Process group that spawned processes should join, if any.
    process_group: Option<ProcessGroupRef>,
}

/// Parameters for execution.
#[derive(Clone, Default)]
pub struct ExecutionParameters {
    /// The open files tracked by the current context.
    open_files: openfiles::OpenFiles,
    /// Policy for how to manage spawned external processes.
    pub process_group_policy: ProcessGroupPolicy,
    /// Whether `errexit` (exit on error) behavior should be
    /// suppressed in this execution context. Defaults to `false`.
    pub suppress_errexit: bool,
    /// Process supervisor to use for external commands in this context.
    ///
    /// When absent, the shell's installed supervisor (if any) is used.
    process_supervisor: Option<Arc<dyn ProcessSupervisor>>,
    /// Cooperative cancellation signal provided by the embedding host.
    cancel_token: Option<tokio_util::sync::CancellationToken>,
    /// Permission to spawn cleanup children while a cancellation cleanup trap runs.
    cleanup_permit: Option<Arc<dyn CleanupPermit>>,
    /// Observer that receives pipeline structure and lifecycle information.
    pipeline_observer: Option<Arc<dyn PipelineObserver>>,
    /// Nearest enclosing multi-stage pipeline occurrence, if any.
    pipeline_parent: Option<u64>,
    /// 当前语法单元及父执行身份，clone 后仍指向同一份解析来源。
    pub(crate) observation_unit: Option<u64>,
    pub(crate) observation_command: Option<u64>,
    pub(crate) observation_source: Option<Arc<str>>,
}

#[allow(clippy::missing_const_for_fn)]
impl ExecutionParameters {
    /// Installs the process supervisor to use for this execution context.
    pub fn set_process_supervisor(&mut self, supervisor: Arc<dyn ProcessSupervisor>) {
        self.process_supervisor = Some(supervisor);
    }

    /// Returns the process supervisor configured on this context, if any.
    pub const fn process_supervisor(&self) -> Option<&Arc<dyn ProcessSupervisor>> {
        self.process_supervisor.as_ref()
    }

    /// Installs the cooperative cancellation token for this execution context.
    pub fn set_cancel_token(&mut self, token: tokio_util::sync::CancellationToken) {
        self.cancel_token = Some(token);
    }

    /// Returns the cooperative cancellation token for this execution context.
    pub const fn cancel_token(&self) -> Option<&tokio_util::sync::CancellationToken> {
        self.cancel_token.as_ref()
    }

    /// Returns whether the embedding host has requested cancellation.
    pub fn is_cancellation_requested(&self) -> bool {
        self.cancel_token
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    }

    /// Installs the cleanup permit used for cleanup traps.
    pub fn set_cleanup_permit(&mut self, permit: Option<Arc<dyn CleanupPermit>>) {
        self.cleanup_permit = permit;
    }

    /// Returns the cleanup permit for this execution context, if any.
    pub const fn cleanup_permit(&self) -> Option<&Arc<dyn CleanupPermit>> {
        self.cleanup_permit.as_ref()
    }

    /// Installs the observer that receives pipeline structure and lifecycle
    /// information.
    pub fn set_pipeline_observer(&mut self, observer: Arc<dyn PipelineObserver>) {
        self.pipeline_observer = Some(observer);
    }

    /// Returns the pipeline observer configured on this context, if any.
    pub const fn pipeline_observer(&self) -> Option<&Arc<dyn PipelineObserver>> {
        self.pipeline_observer.as_ref()
    }

    /// Sets the nearest enclosing multi-stage pipeline occurrence, if any.
    pub fn set_pipeline_parent(&mut self, parent: Option<u64>) {
        self.pipeline_parent = parent;
    }

    /// Returns the nearest enclosing multi-stage pipeline occurrence, if any.
    pub const fn pipeline_parent(&self) -> Option<u64> {
        self.pipeline_parent
    }

    pub(crate) fn observe_skipped(&self, span: Option<crate::SourceSpan>) {
        if let Some(observer) = &self.pipeline_observer {
            observer.skipped(self.observation_unit, span);
        }
    }

    pub(crate) fn observe_iteration(&self, span: Option<crate::SourceSpan>) {
        if let Some(observer) = &self.pipeline_observer {
            observer.iteration(self.observation_unit, span);
        }
    }

    pub(crate) fn observe_resolved(&self, kind: &str) {
        if let (Some(observer), Some(execution)) =
            (&self.pipeline_observer, self.observation_command)
        {
            observer.command_resolved(execution, kind);
        }
    }

    /// Returns the standard input file; usable with `write!` et al.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn stdin(
        &self,
        shell: &Shell<impl extensions::ShellExtensions>,
    ) -> impl std::io::Read + 'static {
        self.try_stdin(shell).unwrap_or_else(|| {
            ioutils::FailingReaderWriter::new("standard input not available").into()
        })
    }

    /// Tries to retrieve the standard input file. Returns `None` if not set.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn try_stdin(&self, shell: &Shell<impl extensions::ShellExtensions>) -> Option<OpenFile> {
        self.try_fd(shell, openfiles::OpenFiles::STDIN_FD)
    }

    /// Returns the standard output file; usable with `write!` et al. In the event that
    /// no such file is available, returns a valid implementation of `std::io::Write`
    /// that fails all I/O requests.
    ///
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn stdout(
        &self,
        shell: &Shell<impl extensions::ShellExtensions>,
    ) -> impl std::io::Write + 'static {
        self.try_stdout(shell).unwrap_or_else(|| {
            ioutils::FailingReaderWriter::new("standard output not available").into()
        })
    }

    /// Tries to retrieve the standard output file. Returns `None` if not set.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn try_stdout(&self, shell: &Shell<impl extensions::ShellExtensions>) -> Option<OpenFile> {
        self.try_fd(shell, openfiles::OpenFiles::STDOUT_FD)
    }

    /// Returns the standard error file; usable with `write!` et al. In the event that
    /// no such file is available, returns a valid implementation of `std::io::Write`
    /// that fails all I/O requests.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn stderr(
        &self,
        shell: &Shell<impl extensions::ShellExtensions>,
    ) -> impl std::io::Write + 'static {
        self.try_stderr(shell).unwrap_or_else(|| {
            ioutils::FailingReaderWriter::new("standard error not available").into()
        })
    }

    /// Tries to retrieve the standard error file. Returns `None` if not set.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn try_stderr(&self, shell: &Shell<impl extensions::ShellExtensions>) -> Option<OpenFile> {
        self.try_fd(shell, openfiles::OpenFiles::STDERR_FD)
    }

    /// Returns the file descriptor with the given number. Returns `None`
    /// if the file descriptor is not open.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    /// * `fd` - The file descriptor number to retrieve.
    pub fn try_fd(
        &self,
        shell: &Shell<impl extensions::ShellExtensions>,
        fd: ShellFd,
    ) -> Option<openfiles::OpenFile> {
        let resolved = match self.open_files.fd_entry(fd) {
            openfiles::OpenFileEntry::Open(f) => Some(f.clone()),
            openfiles::OpenFileEntry::NotPresent => None,
            openfiles::OpenFileEntry::NotSpecified => {
                // We didn't have this fd specified one way or the other; we fallback
                // to what's represented in the shell's open files.
                shell.persistent_open_files().try_fd(fd).cloned()
            }
        };

        if shell.is_embedded() && is_standard_fd(fd) {
            // An embedded shell must never reach the host's own standard streams:
            // an unset (or still host-inherited) logical fd is /dev/null instead.
            return match resolved {
                Some(file) if !file.is_host_stdio() => Some(file),
                _ => openfiles::null().ok(),
            };
        }

        resolved
    }

    /// Sets the given file descriptor to the provided open file.
    ///
    /// # Arguments
    ///
    /// * `fd` - The file descriptor number to set.
    /// * `file` - The open file to set.
    pub fn set_fd(&mut self, fd: ShellFd, file: openfiles::OpenFile) {
        self.open_files.set_fd(fd, file);
    }

    /// Iterates over all open file descriptors in this context.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn iter_fds(
        &self,
        shell: &Shell<impl extensions::ShellExtensions>,
    ) -> impl Iterator<Item = (ShellFd, openfiles::OpenFile)> {
        let our_fds = self.open_files.iter_fds();
        let shell_fds = shell
            .persistent_open_files()
            .iter_fds()
            .filter(|(fd, _)| !self.open_files.contains_fd(*fd));

        #[allow(clippy::needless_collect)]
        let all_fds: Vec<_> = our_fds
            .chain(shell_fds)
            .map(|(fd, file)| (fd, file.clone()))
            .collect();

        all_fds.into_iter()
    }
}

#[derive(Clone, Debug, Default)]
/// Policy for how to manage spawned external processes.
pub enum ProcessGroupPolicy {
    /// Place the process in a new process group.
    #[default]
    NewProcessGroup,
    /// Place the process in the same process group as its parent.
    SameProcessGroup,
}

#[async_trait::async_trait]
pub trait Execute {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error>;
}

#[async_trait::async_trait]
trait ExecuteInPipeline<SE: extensions::ShellExtensions> {
    async fn execute_in_pipeline(
        &self,
        context: PipelineExecutionContext<'_, SE>,
        params: ExecutionParameters,
    ) -> Result<ExecutionSpawnResult, error::Error>;
}

#[async_trait::async_trait]
impl Execute for ast::Program {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let mut result = ExecutionResult::success();

        for command in &self.complete_commands {
            // Stop promptly when the embedding host has cancelled this run.
            if let Some(cancelled) = shell.check_interpreter_boundary(params).await? {
                result = cancelled;
                break;
            }

            // Execute the command and handle any errors without immediately propagating them.
            // This allows interactive shells to continue executing subsequent commands even after
            // errors.
            match command.execute(shell, params).await {
                Ok(exec_result) => result = exec_result,
                Err(err) => {
                    // Display the error and convert to an execution result.
                    let _ = shell.display_error(&mut params.stderr(shell), &err);
                    result = err.into_result(shell);
                }
            }

            // Update status
            shell.set_last_exit_status(result.exit_code.into());

            // Check if we should stop executing subsequent commands
            if !result.is_normal_flow() {
                break;
            }
        }

        Ok(result)
    }
}

#[async_trait::async_trait]
impl Execute for ast::CompoundList {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let mut result = ExecutionResult::success();

        for ast::CompoundListItem(ao_list, sep) in &self.0 {
            let run_async = matches!(sep, ast::SeparatorOperator::Async);

            if run_async {
                if let Some(job) = spawn_async_external_pipeline(ao_list, shell, params).await? {
                    let job_formatted = job.to_pid_style_string();

                    if shell.options().interactive && !shell.is_subshell() {
                        writeln!(params.stderr(shell), "{job_formatted}")?;
                    }
                } else {
                    let job = spawn_async_ao_list_in_task(ao_list, shell, params);
                    let job_formatted = job.to_pid_style_string();

                    if shell.options().interactive && !shell.is_subshell() {
                        writeln!(params.stderr(shell), "{job_formatted}")?;
                    }
                }

                result = ExecutionResult::success();
            } else {
                result = ao_list.execute(shell, params).await?;

                // Update status
                shell.set_last_exit_status(result.exit_code.into());
            }

            if !result.is_normal_flow() {
                break;
            }
        }

        Ok(result)
    }
}

fn spawn_async_ao_list_in_task<'a, SE: extensions::ShellExtensions>(
    ao_list: &ast::AndOrList,
    shell: &'a mut Shell<SE>,
    params: &ExecutionParameters,
) -> &'a jobs::Job {
    // Clone the inputs.
    let mut cloned_shell = shell.clone();
    let mut cloned_params = params.clone();
    let cloned_ao_list = ao_list.clone();

    // Mark the child shell as not interactive; we don't want it messing with the terminal too much.
    cloned_shell.options_mut().interactive = false;

    // Redirect stdin to null, per spec.
    if let Ok(null) = openfiles::null() {
        cloned_params.set_fd(openfiles::OpenFiles::STDIN_FD, null);
    }

    let join_handle = tokio::spawn(async move {
        cloned_ao_list
            .execute(&mut cloned_shell, &cloned_params)
            .await
    });

    shell.jobs_mut().add_as_current(jobs::Job::new(
        [jobs::JobTask::Internal(join_handle)],
        ao_list.to_string(),
        jobs::JobState::Running,
    ))
}

/// Returns the literal command name of a word, or `None` when the word could
/// expand to something other than a fixed name.
fn static_command_name(word: &ast::Word) -> Option<String> {
    let text = word.flatten();

    if text.is_empty() {
        return None;
    }

    let is_plain = text.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '+' | ':' | '@' | '%')
    });

    is_plain.then_some(text)
}

/// Returns the pipeline of a background and-or list when every stage is a plain
/// external command that can be spawned with a real process ID.
fn statically_external_pipeline<'a, SE: extensions::ShellExtensions>(
    ao_list: &'a ast::AndOrList,
    shell: &Shell<SE>,
) -> Option<&'a ast::Pipeline> {
    if !ao_list.additional.is_empty() {
        return None;
    }

    let pipeline = &ao_list.first;
    if pipeline.bang || pipeline.seq.is_empty() {
        return None;
    }

    for command in &pipeline.seq {
        let ast::Command::Simple(simple) = command else {
            return None;
        };

        let name = static_command_name(simple.word_or_name.as_ref()?)?;

        // Anything that could resolve to a builtin, function or alias is handled
        // by the ordinary logical background task path instead.
        if shell.aliases().contains_key(&name)
            || shell.funcs().get(name.as_str()).is_some()
            || shell.builtins().get(&name).is_some_and(|r| !r.disabled)
        {
            return None;
        }

        if simple
            .prefix
            .iter()
            .flat_map(|prefix| prefix.0.iter())
            .chain(simple.suffix.iter().flat_map(|suffix| suffix.0.iter()))
            .any(|item| matches!(item, CommandPrefixOrSuffixItem::ProcessSubstitution(..)))
        {
            return None;
        }
    }

    Some(pipeline)
}

/// Starts a background pipeline made up exclusively of plain external commands.
///
/// The processes are spawned and registered before this returns, so `$!` can
/// report a real PID for the job. Returns `None` when the and-or list cannot be
/// proven to consist only of external commands, in which case the caller falls
/// back to a logical background task without an operating-system identity.
async fn spawn_async_external_pipeline<'a>(
    ao_list: &ast::AndOrList,
    shell: &'a mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
) -> Result<Option<&'a jobs::Job>, error::Error> {
    let Some(pipeline) = statically_external_pipeline(ao_list, shell) else {
        return Ok(None);
    };

    // This path bypasses `ast::Pipeline::execute`, so it must observe the
    // occurrence itself, before any process is spawned. A single-stage
    // background command has no occurrence of its own.
    let occurrence = begin_pipeline_occurrence(pipeline, shell, params)?;
    label_pipeline_stages(pipeline, params.pipeline_observer(), occurrence)?;

    let (mut stage_inputs, mut stage_outputs) =
        create_pipeline_pipes(pipeline, params.pipeline_observer(), occurrence)?;

    let mut tasks = Vec::new();
    let mut observed_stages = Vec::new();
    let mut last_completed: Option<ExecutionResult> = None;
    let mut process_group: Option<ProcessGroupRef> = None;
    let mut managed_group: Option<ProcessGroupRef> = None;

    for (index, command) in pipeline.seq.iter().enumerate() {
        let mut cmd_params = params.clone();
        cmd_params.process_group_policy = if index == 0 {
            ProcessGroupPolicy::NewProcessGroup
        } else {
            ProcessGroupPolicy::SameProcessGroup
        };

        // Stages of this occurrence see it as their nearest multi-stage parent.
        if let Some(id) = occurrence {
            cmd_params.set_pipeline_parent(Some(id));
        }

        if let Some(reader) = stage_inputs[index].take() {
            cmd_params.open_files.set_fd(OpenFiles::STDIN_FD, reader);
        }
        if let Some(writer) = stage_outputs[index].take() {
            cmd_params.open_files.set_fd(OpenFiles::STDOUT_FD, writer);
        }

        // Background jobs read from /dev/null unless they redirect stdin
        // themselves, matching the logical background task path.
        if index == 0
            && let Ok(null) = openfiles::null()
        {
            cmd_params.open_files.set_fd(OpenFiles::STDIN_FD, null);
        }

        // Each stage runs in a throwaway shell so that it always starts a child
        // process rather than executing inline in the background shell.
        let mut stage_shell = shell.clone();
        stage_shell.options_mut().interactive = false;

        let context = PipelineExecutionContext {
            shell: commands::ShellForCommand::ParentShell(&mut stage_shell),
            process_group: process_group.clone(),
        };

        let spawn_result = command.execute_in_pipeline(context, cmd_params).await?;

        match spawn_result {
            ExecutionSpawnResult::StartedProcess(mut child) => {
                if process_group.is_none() {
                    process_group = child.group_ref();
                    // The job keeps this lease so `kill %1` can still reach the
                    // group after the per-child leases are released.
                    managed_group = process_group.clone();
                }

                child.release_group_lease();
                tasks.push(jobs::JobTask::External(child));
                observed_stages.push(index + 1);
            }
            ExecutionSpawnResult::Completed(completed) => {
                // A stage that never started a process (for example, a command
                // that could not be executed) still has a real result.
                if let (Some(observer), Some(id)) = (params.pipeline_observer(), occurrence) {
                    observer.stage_done(id, index + 1, u8::from(&completed.exit_code), None)?;
                }
                last_completed = Some(completed);
            }
            ExecutionSpawnResult::StartedTask(_) => {}
        }
    }

    if tasks.is_empty() {
        // Every stage completed without starting a process, which happens when
        // its external command could not be executed at all. The pipeline has
        // already run, so it must not fall back to the logical background task
        // path: that would execute it a second time and leave the observed
        // occurrence unfinished. Register the observed outcome as a completed
        // job instead. The pre-check only admits plain external commands, so a
        // completed result is always available here.
        let Some(completed) = last_completed else {
            return Ok(None);
        };

        if let (Some(observer), Some(id)) = (params.pipeline_observer(), occurrence) {
            observer.finish(id, u8::from(&completed.exit_code))?;
        }

        let job = jobs::Job::new(
            [jobs::JobTask::Internal(tokio::spawn(async move {
                Ok(completed)
            }))],
            ao_list.to_string(),
            jobs::JobState::Running,
        );

        return Ok(Some(shell.jobs_mut().add_as_current(job)));
    }

    let mut job = jobs::Job::new(tasks, ao_list.to_string(), jobs::JobState::Running);

    if let Some(group) = &managed_group {
        job.set_managed_group(group);
    }

    // The job reaps the external stages, so it reports their completion. Stages
    // that did not start a process are left unreported for the run to mark as
    // incomplete.
    if let (Some(observer), Some(id)) = (params.pipeline_observer(), occurrence) {
        job.set_pipeline_observation(Arc::clone(observer), id, observed_stages);
    }

    Ok(Some(shell.jobs_mut().add_as_current(job)))
}

#[async_trait::async_trait]
impl Execute for ast::AndOrList {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let has_operators = !self.additional.is_empty();

        // For the first command, suppress errexit if there are more commands after it
        let mut first_params = params.clone();
        if has_operators {
            first_params.suppress_errexit = true;
        }

        let mut result = self.first.execute(shell, &first_params).await?;

        for (index, next_ao) in self.additional.iter().enumerate() {
            if let Some(cancelled) = shell.check_interpreter_boundary(params).await? {
                return Ok(cancelled);
            }

            // Check for non-normal control flow.
            if !result.is_normal_flow() {
                break;
            }

            let (is_and, pipeline) = match next_ao {
                ast::AndOr::And(p) => (true, p),
                ast::AndOr::Or(p) => (false, p),
            };

            // If we short-circuit, then we don't break out of the whole loop
            // but we skip evaluating the current pipeline. We'll then continue
            // on and possibly evaluate a subsequent one (depending on the
            // operator before it).
            if is_and {
                if !result.is_success() {
                    params.observe_skipped(pipeline.location());
                    continue;
                }
            } else if result.is_success() {
                params.observe_skipped(pipeline.location());
                continue;
            }

            // For the last command in the chain, use original params (errexit not suppressed)
            // For earlier commands, suppress errexit
            let mut params = params.clone();

            let is_last = index == self.additional.len() - 1;
            if !is_last {
                params.suppress_errexit = true;
            }

            result = pipeline.execute(shell, &params).await?;
        }

        Ok(result)
    }
}

#[async_trait::async_trait]
impl Execute for ast::Pipeline {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        // Capture current timing if so requested.
        let stopwatch = self
            .timed
            .is_some()
            .then(timing::start_timing)
            .transpose()?;

        let mut params = params.clone();

        // If this pipeline is negated, suppress errexit for commands within it
        if self.bang {
            params.suppress_errexit = true;
        }

        if let Some(cancelled) = shell.check_interpreter_boundary(&params).await? {
            return Ok(cancelled);
        }

        // Start observing this pipeline occurrence (if it has more than one
        // stage) before any process is spawned. Nested pipelines in the stages
        // inherit this occurrence as their nearest multi-stage parent.
        let occurrence = begin_pipeline_occurrence(self, shell, &params)?;
        if let Some(id) = occurrence {
            params.set_pipeline_parent(Some(id));
        }
        label_pipeline_stages(self, params.pipeline_observer(), occurrence)?;

        // Spawn all the processes required for the pipeline, connecting outputs/inputs with pipes
        // as needed.
        let (spawn_results, managed_group) =
            spawn_pipeline_processes(self, shell, &params, occurrence).await?;

        // Wait for the processes. This also has a side effect of updating pipeline status.
        let mut result = wait_for_pipeline_processes_and_update_status(
            self,
            spawn_results,
            managed_group.as_ref(),
            occurrence,
            shell,
            &params,
        )
        .await?;

        // A cancelled run stops here: the exit code is the real terminal status,
        // it must not be inverted by `!`, and no ERR trap or further
        // interpretation may run.
        if params.is_cancellation_requested() {
            shell.set_last_exit_status(result.exit_code.into());
            return Ok(result);
        }

        // Invert the exit code if requested.
        if self.bang {
            result.exit_code = ExecutionExitCode::from(if result.is_success() { 1 } else { 0 });
            // The negation's status is a shell-level decision, not the observed
            // termination of the pipeline.
            result.signal = None;
        }

        // Update exit status.
        shell.set_last_exit_status(result.exit_code.into());

        // Report the observed pipeline only after `pipefail` and `!` handling so
        // that its final status matches what the shell reports, and before the
        // ERR trap runs.
        if let (Some(observer), Some(id)) = (params.pipeline_observer(), occurrence) {
            observer.finish(id, u8::from(&result.exit_code))?;
        }

        // Fire the ERR trap if the pipeline failed in a non-conditional context.
        // We reuse `suppress_errexit` here because bash suppresses the ERR trap in
        // exactly the same contexts it suppresses errexit (conditionals, `!`-prefixed
        // pipelines, etc.).
        // A logical shell replacement is not a failure of the replaced shell, so
        // neither the ERR trap nor errexit applies to it.
        if !result.is_success()
            && !params.suppress_errexit
            && !self.bang
            && !result.is_exec_replaced()
        {
            if shell.traps().handles(crate::traps::TrapSignal::Err) {
                shell
                    .invoke_trap_handler(crate::traps::TrapSignal::Err, &params)
                    .await?;
            }
        }

        // Apply errexit if not suppressed (and not negated)
        if !params.suppress_errexit && !self.bang {
            shell.apply_errexit_if_enabled(&mut result);
        }

        // If requested, report timing.
        if let (Some(timed), Some(stopwatch)) = (&self.timed, &stopwatch)
            && let Some(mut stderr) = params.try_fd(shell, openfiles::OpenFiles::STDERR_FD)
        {
            let timing = stopwatch.stop()?;
            if timed.is_posix_output() {
                std::write!(
                    stderr,
                    "real {}\nuser {}\nsys {}\n",
                    timing::format_duration_posixly(&timing.wall),
                    timing::format_duration_posixly(&timing.user),
                    timing::format_duration_posixly(&timing.system),
                )?;
            } else {
                std::write!(
                    stderr,
                    "\nreal\t{}\nuser\t{}\nsys\t{}\n",
                    timing::format_duration_non_posixly(&timing.wall),
                    timing::format_duration_non_posixly(&timing.user),
                    timing::format_duration_non_posixly(&timing.system),
                )?;
            }
        }

        Ok(result)
    }
}

/// Consumes an `ExecReplaced` transition reported by a pipeline stage that ran in
/// its own subshell.
///
/// The stage reports only the replacement's exit status; the parent shell keeps
/// running. `StartedTask` results are mapped in place so the consumption also
/// applies to builtins executed in an owned shell.
fn consume_stage_exec_replaced(spawn_result: ExecutionSpawnResult) -> ExecutionSpawnResult {
    match spawn_result {
        ExecutionSpawnResult::Completed(result) => {
            ExecutionSpawnResult::Completed(consume_exec_replaced_result(result))
        }
        ExecutionSpawnResult::StartedTask(handle) => {
            ExecutionSpawnResult::StartedTask(tokio::spawn(async move {
                let result = handle.await??;
                Ok(consume_exec_replaced_result(result))
            }))
        }
        other => other,
    }
}

/// Clears an `ExecReplaced` transition, keeping the replacement's exit status.
fn consume_exec_replaced_result(mut result: ExecutionResult) -> ExecutionResult {
    if result.is_exec_replaced() {
        result.next_control_flow = crate::ExecutionControlFlow::Normal;
    }

    result
}

/// Starts observing a pipeline, returning its occurrence identifier.
///
/// Only pipelines with more than one stage produce an occurrence; single-stage
/// wrappers pass the nearest enclosing multi-stage occurrence through unchanged.
fn begin_pipeline_occurrence(
    pipeline: &ast::Pipeline,
    shell: &Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
) -> Result<Option<u64>, error::Error> {
    if pipeline.seq.len() <= 1 {
        return Ok(None);
    }

    let Some(observer) = params.pipeline_observer() else {
        return Ok(None);
    };

    Ok(Some(observer.begin(
        params.pipeline_parent(),
        pipeline.seq.len(),
        pipeline.bang,
        shell.options().return_last_failure_from_pipeline,
    )?))
}

/// Records the command text of every stage of a pipeline occurrence.
fn label_pipeline_stages(
    pipeline: &ast::Pipeline,
    observer: Option<&Arc<dyn PipelineObserver>>,
    occurrence: Option<u64>,
) -> Result<(), error::Error> {
    let (Some(observer), Some(id)) = (observer, occurrence) else {
        return Ok(());
    };

    for (index, command) in pipeline.seq.iter().enumerate() {
        observer.label(id, index + 1, &command.to_string())?;
    }

    Ok(())
}

/// Creates the pipe endpoints between adjacent pipeline stages.
///
/// The returned vectors are indexed by 0-based stage: entry `i` of the first is
/// the standard input of stage `i` (when this pipeline pipes into it) and entry
/// `i` of the second is its standard output. The first stage and the last stage
/// keep the caller's standard input and output respectively. Edges are observed
/// in ascending order, so edge `e` always connects stage `e` to stage `e + 1`.
fn create_pipeline_pipes(
    pipeline: &ast::Pipeline,
    observer: Option<&Arc<dyn PipelineObserver>>,
    occurrence: Option<u64>,
) -> Result<(Vec<Option<OpenFile>>, Vec<Option<OpenFile>>), error::Error> {
    let pipeline_len = pipeline.seq.len();
    let mut stage_inputs: Vec<Option<OpenFile>> = vec![None; pipeline_len];
    let mut stage_outputs: Vec<Option<OpenFile>> = vec![None; pipeline_len];

    for edge in 1..pipeline_len {
        let producer_stage = edge - 1;
        let consumer_stage = edge;

        let tapped = match (observer, occurrence) {
            (Some(observer), Some(id)) => observer.pipe(
                id,
                edge,
                &pipeline.seq[producer_stage].to_string(),
                &pipeline.seq[consumer_stage].to_string(),
            )?,
            _ => None,
        };

        let (reader, writer) = match tapped {
            Some((reader, writer)) => (reader, writer),
            None => {
                let (reader, writer) = std::io::pipe()?;
                (reader.into(), writer.into())
            }
        };

        stage_inputs[consumer_stage] = Some(reader);
        stage_outputs[producer_stage] = Some(writer);
    }

    Ok((stage_inputs, stage_outputs))
}

async fn spawn_pipeline_processes(
    pipeline: &ast::Pipeline,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    occurrence: Option<u64>,
) -> Result<(VecDeque<ExecutionSpawnResult>, Option<ProcessGroupRef>), error::Error> {
    let pipeline_len = pipeline.seq.len();
    let mut spawn_results = VecDeque::new();
    let mut process_group: Option<ProcessGroupRef> = None;

    // Create pipes to use between commands. A multi-stage pipeline mirrors each
    // adjacent edge through the observer, which may supply its own endpoints.
    let (mut stage_inputs, mut stage_outputs) =
        create_pipeline_pipes(pipeline, params.pipeline_observer(), occurrence)?;

    for (current_pipeline_index, command) in pipeline.seq.iter().enumerate() {
        //
        // We run a command directly in the current shell if either of the following is true:
        //     * There's only one command in the pipeline.
        //     * This is the *last* command in the pipeline, the lastpipe option is enabled, and job
        //       monitoring is disabled.
        // Otherwise, we spawn a separate subshell for each command in the pipeline.
        //

        let run_in_current_shell = pipeline_len == 1
            || (current_pipeline_index == pipeline_len - 1
                && shell.options().run_last_pipeline_cmd_in_current_shell
                && !shell.options().enable_job_control);

        // Set up parameters appropriate for this command.
        let mut cmd_params = params.clone();

        // Install this stage's end of the adjacent pipes.
        if let Some(reader) = stage_inputs[current_pipeline_index].take() {
            cmd_params.open_files.set_fd(OpenFiles::STDIN_FD, reader);
        }
        if let Some(writer) = stage_outputs[current_pipeline_index].take() {
            cmd_params.open_files.set_fd(OpenFiles::STDOUT_FD, writer);
        }

        let pipeline_context = if !run_in_current_shell {
            // Make sure that all commands in the pipeline are in the same process group.
            if current_pipeline_index > 0 {
                cmd_params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;
            }

            PipelineExecutionContext {
                shell: commands::ShellForCommand::OwnedShell {
                    target: Box::new(shell.clone()),
                    parent: shell,
                },
                process_group: process_group.clone(),
            }
        } else {
            PipelineExecutionContext {
                shell: commands::ShellForCommand::ParentShell(shell),
                process_group: process_group.clone(),
            }
        };

        let spawn_result = command
            .execute_in_pipeline(pipeline_context, cmd_params)
            .await?;

        // A logical shell replacement ends only the stage's own subshell. When
        // the stage runs in the parent shell instead (single-command pipelines
        // and `lastpipe`), the transition is preserved so it can end the shell.
        let spawn_result = if run_in_current_shell {
            spawn_result
        } else {
            consume_stage_exec_replaced(spawn_result)
        };

        // Adopt the group of the first stage that actually spawned a process; it
        // cannot be assumed that the first stage is an external program.
        if let ExecutionSpawnResult::StartedProcess(child) = &spawn_result
            && process_group.is_none()
        {
            process_group = child.group_ref();
        }

        spawn_results.push_back(spawn_result);
    }

    // The pipeline is fully assembled, so the per-child assembly leases are no
    // longer needed. The pipeline's own handle is kept until the wait finishes:
    // a stopped pipeline hands it to the job, and a completed one drops it here.
    for result in &mut spawn_results {
        if let ExecutionSpawnResult::StartedProcess(child) = result {
            child.release_group_lease();
        }
    }

    Ok((spawn_results, process_group))
}

async fn wait_for_pipeline_processes_and_update_status(
    pipeline: &ast::Pipeline,
    mut process_spawn_results: VecDeque<ExecutionSpawnResult>,
    managed_group: Option<&ProcessGroupRef>,
    occurrence: Option<u64>,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
) -> Result<ExecutionResult, error::Error> {
    let mut result = ExecutionResult::success();
    let mut stopped_children = vec![];
    let mut last_failure: Option<(ExecutionExitCode, Option<i32>)> = None;
    let mut stage_index = 0usize;

    // Clear our the pipeline status so we can start filling it out.
    shell.last_pipeline_statuses_mut().clear();

    while let Some(child) = process_spawn_results.pop_front() {
        // Stop waiting once the host has cancelled the run; the supervisor still
        // reaps whatever remains, so dropping the handles leaks no children.
        if let Some(cancelled) = shell.check_interpreter_boundary(params).await? {
            result = cancelled;
            break;
        }

        // Stages are popped in pipeline order, so the counter is the 1-based
        // stage index for the result that is about to arrive.
        stage_index += 1;

        // Only a directly reaped external stage can report a real signal; a
        // builtin, function, compound, or task stage reports its shell status
        // alone.
        let direct_external = matches!(&child, ExecutionSpawnResult::StartedProcess(_));

        let wait_result = if !stopped_children.is_empty() {
            child.poll().await?
        } else {
            child.wait().await?
        };

        match wait_result {
            ExecutionWaitResult::Completed(current_result) => {
                if let (Some(observer), Some(id)) = (params.pipeline_observer(), occurrence) {
                    let signal = if direct_external {
                        current_result.signal
                    } else {
                        None
                    };
                    observer.stage_done(
                        id,
                        stage_index,
                        u8::from(&current_result.exit_code),
                        signal,
                    )?;
                }

                result = current_result;
                shell.set_last_exit_status(result.exit_code.into());
                shell
                    .last_pipeline_statuses_mut()
                    .push(result.exit_code.into());

                // Track the last failure for pipefail option
                if !result.is_success() {
                    last_failure = Some((result.exit_code, result.signal));
                }
            }
            ExecutionWaitResult::Stopped(child) => {
                result = ExecutionResult::stopped();
                shell.set_last_exit_status(result.exit_code.into());
                shell
                    .last_pipeline_statuses_mut()
                    .push(result.exit_code.into());

                stopped_children.push(jobs::JobTask::External(child));
            }
        }
    }

    // Apply pipefail semantics if enabled. A cancelled run keeps its own terminal
    // status instead of adopting an earlier stage's failure code.
    if !params.is_cancellation_requested() && shell.options().return_last_failure_from_pipeline {
        if let Some((failure_exit_code, failure_signal)) = last_failure {
            result.exit_code = failure_exit_code;
            result.signal = failure_signal;
        }
    }

    if shell.options().interactive && !shell.is_embedded() {
        sys::terminal::move_self_to_foreground()?;
    }

    // If there were stopped jobs, then encapsulate the pipeline as a managed job and hand it
    // off to the job manager.
    if !stopped_children.is_empty() {
        let mut stopped_job = jobs::Job::new(
            stopped_children,
            pipeline.to_string(),
            jobs::JobState::Stopped,
        );

        if let Some(group) = managed_group {
            stopped_job.set_managed_group(group);
        }

        let job = shell.jobs_mut().add_as_current(stopped_job);

        let formatted = job.to_string();

        // N.B. We use the '\r' to overwrite any ^Z output.
        writeln!(params.stderr(shell), "\r{formatted}")?;
    }

    Ok(result)
}

#[async_trait::async_trait]
impl<SE: extensions::ShellExtensions> ExecuteInPipeline<SE> for ast::Command {
    async fn execute_in_pipeline(
        &self,
        mut pipeline_context: PipelineExecutionContext<'_, SE>,
        mut params: ExecutionParameters,
    ) -> Result<ExecutionSpawnResult, error::Error> {
        if let Some(cancelled) = pipeline_context
            .shell
            .check_interpreter_boundary(&params)
            .await?
        {
            return Ok(ExecutionSpawnResult::Completed(cancelled));
        }

        if pipeline_context.shell.options().do_not_execute_commands {
            return Ok(ExecutionSpawnResult::Completed(ExecutionResult::success()));
        }

        let mut observation = params.pipeline_observer().and_then(|observer| {
            observer
                .command_begin(params.observation_unit, self, params.observation_command)
                .map(|execution| {
                    let observation = crate::pipeline::CommandObservation::new(
                        Arc::clone(observer),
                        execution,
                        params.cancel_token().cloned(),
                    );
                    (execution, observation)
                })
        });
        if let Some((execution, _)) = &observation {
            params.observation_command = Some(*execution);
        }
        let result = execute_command_body(self, pipeline_context, params).await;
        if let Some((_, mut observation)) = observation.take() {
            return match result {
                Ok(ExecutionSpawnResult::Completed(result)) => {
                    observation.finish(&result);
                    Ok(ExecutionSpawnResult::Completed(result))
                }
                Ok(ExecutionSpawnResult::StartedProcess(mut child)) => {
                    child.set_command_observation(observation);
                    Ok(ExecutionSpawnResult::StartedProcess(child))
                }
                Ok(ExecutionSpawnResult::StartedTask(task)) => Ok(
                    ExecutionSpawnResult::StartedTask(tokio::spawn(async move {
                        let result = task.await?;
                        if let Ok(result) = &result {
                            observation.finish(result);
                        }
                        result
                    })),
                ),
                Err(error) => {
                    // 错误转换为 shell 状态的规则仍由原错误类型负责。
                    let result = ExecutionResult::new(ExecutionExitCode::from(&error).into());
                    observation.finish(&result);
                    Err(error)
                }
            };
        }
        result
    }
}

async fn execute_command_body<SE: extensions::ShellExtensions>(
    command: &ast::Command,
    mut pipeline_context: PipelineExecutionContext<'_, SE>,
    mut params: ExecutionParameters,
) -> Result<ExecutionSpawnResult, error::Error> {
    // Updates the shell with information about the currently executing command.
    pipeline_context.shell.set_current_cmd(command);

    match command {
        ast::Command::Simple(simple) => simple.execute_in_pipeline(pipeline_context, params).await,
        ast::Command::Compound(compound, redirects) => {
            // Set up any additional redirects.
            if let Some(redirects) = redirects {
                for redirect in &redirects.0 {
                    setup_redirect(&mut pipeline_context.shell, &mut params, redirect).await?;
                }
            }

            // An owned pipeline stage must start producing before later
            // stages are assembled: a compound body can block on its output
            // pipe (or wait for a sibling stage) for an unbounded time. Run
            // it as a concurrent logical task and hand the handle back, just
            // like an external stage hands back its child process.
            match pipeline_context.shell {
                commands::ShellForCommand::OwnedShell { target, .. } => {
                    let compound = compound.clone();
                    let handle = tokio::spawn(async move {
                        let mut shell = *target;
                        compound.execute(&mut shell, &params).await
                    });
                    Ok(ExecutionSpawnResult::StartedTask(handle))
                }
                commands::ShellForCommand::ParentShell(shell) => {
                    Ok(compound.execute(shell, &params).await?.into())
                }
            }
        }
        ast::Command::Function(func) => Ok(func
            .execute(&mut pipeline_context.shell, &params)
            .await?
            .into()),
        ast::Command::ExtendedTest(e, redirects) => {
            // Set up any additional redirects.
            if let Some(redirects) = redirects {
                for redirect in &redirects.0 {
                    setup_redirect(&mut pipeline_context.shell, &mut params, redirect).await?;
                }
            }

            // Evaluate the extended test expression.
            let result = if extendedtests::eval_extended_test_expr(
                &e.expr,
                &mut pipeline_context.shell,
                &params,
            )
            .await?
            {
                0
            } else {
                1
            };
            Ok(ExecutionResult::new(result).into())
        }
    }
}

enum WhileOrUntil {
    While,
    Until,
}

#[async_trait::async_trait]
impl Execute for ast::CompoundCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        match self {
            Self::BraceGroup(ast::BraceGroupCommand { list, .. }) => {
                list.execute(shell, params).await
            }
            Self::Subshell(ast::SubshellCommand { list, .. }) => {
                if let Some(cancelled) = shell.check_interpreter_boundary(params).await? {
                    return Ok(cancelled);
                }

                // Clone off a new subshell, and run the body of the subshell there.
                // TODO(source-info): Do we need to reset the line number?
                let mut subshell = shell.clone();

                // Handle errors within the subshell context to prevent fatal errors
                // from propagating to the parent shell.
                let subshell_result = match list.execute(&mut subshell, params).await {
                    Ok(result) => result,
                    Err(error) => {
                        // Display the error to stderr, but prevent fatal error propagation
                        let mut stderr = params.stderr(shell);
                        let _ = shell.display_error(&mut stderr, &error);

                        // Convert error to result in subshell context
                        error.into_result(&subshell)
                    }
                };

                // Preserve the subshell's exit code, but don't honor any of its requests to exit
                // the shell, break out of loops, etc.
                Ok(ExecutionResult::from(subshell_result.exit_code))
            }
            Self::ForClause(f) => f.execute(shell, params).await,
            Self::CaseClause(c) => c.execute(shell, params).await,
            Self::IfClause(i) => i.execute(shell, params).await,
            Self::WhileClause(w) => (WhileOrUntil::While, w).execute(shell, params).await,
            Self::UntilClause(u) => (WhileOrUntil::Until, u).execute(shell, params).await,
            Self::Arithmetic(a) => a.execute(shell, params).await,
            Self::ArithmeticForClause(a) => a.execute(shell, params).await,
            Self::Coprocess(c) => c.execute(shell, params).await,
        }
    }
}

#[async_trait::async_trait]
impl Execute for ast::CoprocessCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        if let Some(cancelled) = shell.check_interpreter_boundary(params).await? {
            return Ok(cancelled);
        }

        if shell.options().do_not_execute_commands {
            return Ok(ExecutionResult::success());
        }

        // Resolve the name of the variable that will receive the coprocess's file descriptors.
        let name = self
            .name
            .as_ref()
            .map_or_else(|| "COPROC".to_string(), |w| w.to_string());

        if !valid_variable_name(&name) {
            writeln!(
                params.stderr(shell),
                "coproc {name}: not a valid identifier"
            )?;
            return Ok(ExecutionExitCode::GeneralError.into());
        }

        // Set up the pipes that we'll use to communicate with the coprocess.
        let (stdin_reader, stdin_writer) = std::io::pipe()?;
        let (stdout_reader, stdout_writer) = std::io::pipe()?;

        // Allocate new fds in the (parent) shell for the read end of the coprocess's stdout
        // and the write end of the coprocess's stdin.
        let stdout_fd = shell.open_files_mut().add(stdout_reader.into())?;
        let stdin_fd = shell.open_files_mut().add(stdin_writer.into())?;

        // Crete a subshell that the coprocess will own and run in.
        let mut child_shell = shell.clone();
        child_shell.options_mut().interactive = false;

        // Setup redirection for the coprocess's shell's stdin/stdout.
        let mut child_params = params.clone();
        child_params
            .open_files
            .set_fd(OpenFiles::STDIN_FD, stdin_reader.into());
        child_params
            .open_files
            .set_fd(OpenFiles::STDOUT_FD, stdout_writer.into());

        let body = self.body.clone();
        let join_handle = tokio::spawn(async move {
            let pipeline_context = PipelineExecutionContext {
                shell: commands::ShellForCommand::ParentShell(&mut child_shell),
                process_group: None,
            };
            let spawn_result = body
                .execute_in_pipeline(pipeline_context, child_params)
                .await?;
            match spawn_result.wait().await? {
                ExecutionWaitResult::Completed(result) => Ok(result),
                ExecutionWaitResult::Stopped(_) => Ok(ExecutionResult::stopped()),
            }
        });

        let job = shell.jobs_mut().add_as_current(jobs::Job::new(
            [jobs::JobTask::Internal(join_handle)],
            format!("coproc {name}"),
            jobs::JobState::Running,
        ));
        let job_id = job.id;

        // Fill out the fd variable.
        let arr_value = ShellValue::from(vec![stdout_fd.to_string(), stdin_fd.to_string()]);
        shell
            .env_mut()
            .set_global(name.clone(), ShellVariable::new(arr_value))?;

        // Set the job ID for the coprocess in a separate variable with the _PID suffix.
        let pid_name = format!("{name}_PID");
        shell
            .env_mut()
            .set_global(pid_name, ShellVariable::new(job_id.to_string()))?;

        Ok(ExecutionResult::success())
    }
}

#[async_trait::async_trait]
impl Execute for ast::ForClauseCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let mut result = ExecutionResult::success();

        // If we were given explicit words to iterate over, then expand them all, with splitting
        // enabled.
        let mut expanded_values = vec![];
        if let Some(unexpanded_values) = &self.values {
            for value in unexpanded_values {
                let mut expanded =
                    expansion::full_expand_and_split_word(shell, params, value).await?;
                expanded_values.append(&mut expanded);
            }
        } else {
            // Otherwise, we use the current positional parameters.
            expanded_values.extend_from_slice(shell.current_shell_args());
        }

        if expanded_values.is_empty() {
            params.observe_skipped(self.body.list.location());
        }
        for value in expanded_values {
            if let Some(cancelled) = shell.check_interpreter_boundary(params).await? {
                return Ok(cancelled);
            }

            if shell.options().print_commands_and_arguments {
                if let Some(unexpanded_values) = &self.values {
                    shell
                        .trace_command(
                            params,
                            std::format!(
                                "for {} in {}",
                                self.variable_name,
                                unexpanded_values.iter().join(" ")
                            ),
                        )
                        .await;
                } else {
                    shell
                        .trace_command(params, std::format!("for {}", self.variable_name))
                        .await;
                }
            }

            // Update the variable.
            shell.env_mut().update_or_add(
                &self.variable_name,
                ShellValueLiteral::Scalar(value),
                |_| Ok(()),
                EnvironmentLookup::Anywhere,
                EnvironmentScope::Global,
            )?;

            params.observe_iteration(self.location());
            result = self.body.list.execute(shell, params).await?;
            if result.is_return_or_exit() {
                break;
            }

            let is_break = result.is_break();

            result.next_control_flow = result.next_control_flow.try_decrement_loop_levels();

            if is_break || result.is_continue() {
                break;
            }
        }

        shell.set_last_exit_status(result.exit_code.into());
        Ok(result)
    }
}

#[async_trait::async_trait]
impl Execute for ast::CaseClauseCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        // N.B. One would think it makes sense to trace the expanded value being switched
        // on, but that's not it.
        if shell.options().print_commands_and_arguments {
            shell
                .trace_command(params, std::format!("case {} in", &self.value))
                .await;
        }

        let expanded_value = expansion::basic_expand_word(shell, params, &self.value).await?;
        let mut result: ExecutionResult = ExecutionResult::success();
        let mut force_execute_next_case = false;

        for (case_index, case) in self.cases.iter().enumerate() {
            if force_execute_next_case {
                force_execute_next_case = false;
            } else {
                let mut matches = false;
                for pattern in &case.patterns {
                    let expanded_pattern = expansion::basic_expand_pattern(shell, params, pattern)
                        .await?
                        .set_extended_globbing(shell.options().extended_globbing)
                        .set_case_insensitive(shell.options().case_insensitive_conditionals);

                    if expanded_pattern.exactly_matches(expanded_value.as_str())? {
                        matches = true;
                        break;
                    }
                }

                if !matches {
                    if let Some(body) = &case.cmd {
                        params.observe_skipped(body.location());
                    }
                    continue;
                }
            }

            result = if let Some(case_cmd) = &case.cmd {
                case_cmd.execute(shell, params).await?
            } else {
                ExecutionResult::success()
            };

            // Check for early return (return/exit) or loop control flow (break/continue)
            if !result.is_normal_flow() {
                break;
            }

            match case.post_action {
                ast::CaseItemPostAction::ExitCase => {
                    for skipped in &self.cases[case_index + 1..] {
                        if let Some(body) = &skipped.cmd {
                            params.observe_skipped(body.location());
                        }
                    }
                    break;
                }
                ast::CaseItemPostAction::UnconditionallyExecuteNextCaseItem => {
                    force_execute_next_case = true;
                }
                ast::CaseItemPostAction::ContinueEvaluatingCases => (),
            }
        }

        shell.set_last_exit_status(result.exit_code.into());

        Ok(result)
    }
}

#[async_trait::async_trait]
impl Execute for ast::IfClauseCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        // Execute condition with errexit suppressed
        let mut condition_params = params.clone();
        condition_params.suppress_errexit = true;
        let condition = self.condition.execute(shell, &condition_params).await?;

        // Check if the condition itself resulted in non-normal control flow.
        if !condition.is_normal_flow() {
            return Ok(condition);
        }

        if condition.is_success() {
            for clause in self.elses.iter().flatten() {
                if let Some(condition) = &clause.condition {
                    params.observe_skipped(condition.location());
                }
                params.observe_skipped(clause.body.location());
            }
            return self.then.execute(shell, params).await;
        }
        params.observe_skipped(self.then.location());

        if let Some(elses) = &self.elses {
            for (index, else_clause) in elses.iter().enumerate() {
                match &else_clause.condition {
                    Some(else_condition) => {
                        let else_condition_result =
                            else_condition.execute(shell, &condition_params).await?;

                        // Check if the elif condition caused non-normal control flow.
                        if !else_condition_result.is_normal_flow() {
                            return Ok(else_condition_result);
                        }

                        if else_condition_result.is_success() {
                            for clause in &elses[index + 1..] {
                                if let Some(condition) = &clause.condition {
                                    params.observe_skipped(condition.location());
                                }
                                params.observe_skipped(clause.body.location());
                            }
                            return else_clause.body.execute(shell, params).await;
                        }
                        params.observe_skipped(else_clause.body.location());
                    }
                    None => {
                        return else_clause.body.execute(shell, params).await;
                    }
                }
            }
        }

        // If we got down here, then no branch was taken; we make sure to
        // reset the last exit status to success and then return success.
        let result = ExecutionResult::success();
        shell.set_last_exit_status(result.exit_code.into());

        Ok(result)
    }
}

#[async_trait::async_trait]
impl Execute for (WhileOrUntil, &ast::WhileOrUntilClauseCommand) {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let is_while = match self.0 {
            WhileOrUntil::While => true,
            WhileOrUntil::Until => false,
        };
        let test_condition = &self.1.0;
        let body = &self.1.1;

        let mut result = ExecutionResult::success();

        // Execute loop condition with errexit suppressed
        let mut condition_params = params.clone();
        condition_params.suppress_errexit = true;

        loop {
            if let Some(cancelled) = shell.check_interpreter_boundary(params).await? {
                return Ok(cancelled);
            }

            let condition_result = test_condition.execute(shell, &condition_params).await?;

            // Update status for condition
            shell.set_last_exit_status(condition_result.exit_code.into());

            if !condition_result.is_normal_flow() {
                result = condition_result;

                // If the condition has break/continue, the while/until loop itself
                // consumes one level. We need to decrement the level before returning.
                result.next_control_flow = result.next_control_flow.try_decrement_loop_levels();
                break;
            }

            if condition_result.is_success() != is_while {
                params.observe_skipped(body.list.location());
                break;
            }

            params.observe_iteration(self.1.location());
            result = body.list.execute(shell, params).await?;
            if result.is_return_or_exit() {
                break;
            }

            let is_break = result.is_break();

            result.next_control_flow = result.next_control_flow.try_decrement_loop_levels();

            if is_break || result.is_continue() {
                break;
            }
        }

        shell.set_last_exit_status(result.exit_code.into());
        Ok(result)
    }
}

#[async_trait::async_trait]
impl Execute for ast::ArithmeticCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let value = self.expr.eval(shell, params, true).await?;
        let result = if value != 0 {
            ExecutionResult::success()
        } else {
            ExecutionResult::general_error()
        };

        shell.set_last_exit_status(result.exit_code.into());

        Ok(result)
    }
}

#[async_trait::async_trait]
impl Execute for ast::ArithmeticForClauseCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let mut result = ExecutionResult::success();
        if let Some(initializer) = &self.initializer {
            initializer.eval(shell, params, true).await?;
        }

        loop {
            if let Some(cancelled) = shell.check_interpreter_boundary(params).await? {
                return Ok(cancelled);
            }

            if let Some(condition) = &self.condition {
                // An empty condition (e.g., `for (( ; ; ))`) means "always true".
                if !condition.value.is_empty() && condition.eval(shell, params, true).await? == 0 {
                    break;
                }
            }

            params.observe_iteration(self.location());
            result = self.body.list.execute(shell, params).await?;
            if result.is_return_or_exit() {
                break;
            }

            let is_break = result.is_break();

            result.next_control_flow = result.next_control_flow.try_decrement_loop_levels();

            if is_break || result.is_continue() {
                break;
            }

            if let Some(updater) = &self.updater {
                updater.eval(shell, params, true).await?;
            }
        }

        shell.set_last_exit_status(result.exit_code.into());
        Ok(result)
    }
}

#[async_trait::async_trait]
impl Execute for ast::FunctionDefinition {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let func_name = self.fname.value.clone();

        // In POSIX mode, function names can't shadow special builtins.
        if shell.options().posix_mode
            && shell
                .builtins()
                .get(&func_name)
                .is_some_and(|r| r.special_builtin)
        {
            return Err(
                error::Error::from(error::ErrorKind::FunctionNameShadowsSpecialBuiltin {
                    name: func_name,
                })
                .into_fatal(),
            );
        }

        // The function definition's source context should be the same as the current frame
        // so we directly pass that through.
        let source_info = shell
            .call_stack()
            .current_frame()
            .map_or_else(crate::SourceInfo::default, |frame| {
                frame.adjusted_source_info()
            });
        shell.define_func(func_name.clone(), self.clone(), &source_info);
        if let Some(registration) = shell.funcs_mut().get_mut(&func_name) {
            registration.set_observation_unit(params.observation_unit);
        }

        let result = ExecutionResult::success();
        shell.set_last_exit_status(result.exit_code.into());

        Ok(result)
    }
}

#[async_trait::async_trait]
#[allow(clippy::too_many_lines)]
impl<SE: extensions::ShellExtensions> ExecuteInPipeline<SE> for ast::SimpleCommand {
    async fn execute_in_pipeline(
        &self,
        mut context: PipelineExecutionContext<'_, SE>,
        mut params: ExecutionParameters,
    ) -> Result<ExecutionSpawnResult, error::Error> {
        let prefix_iter = self.prefix.as_ref().map(|s| s.0.iter()).unwrap_or_default();
        let suffix_iter = self.suffix.as_ref().map(|s| s.0.iter()).unwrap_or_default();
        let cmd_name_items = self
            .word_or_name
            .as_ref()
            .map(|won| CommandPrefixOrSuffixItem::Word(won.clone()));

        let mut assignments = vec![];
        let mut args: Vec<CommandArg> = vec![];
        let mut command_takes_assignments = false;

        // Capture the status change count before expansion, so we can detect
        // if expansion (e.g., command substitution) set an exit status.
        let status_change_count_before_expansion = context.shell.last_exit_status_change_count();

        for item in prefix_iter.chain(cmd_name_items.iter()).chain(suffix_iter) {
            match item {
                CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                    if let Err(e) = setup_redirect(&mut context.shell, &mut params, redirect).await
                    {
                        writeln!(params.stderr(&context.shell), "error: {e}")?;
                        return Ok(ExecutionResult::general_error().into());
                    }
                }
                CommandPrefixOrSuffixItem::ProcessSubstitution(kind, subshell_command) => {
                    let (installed_fd_num, substitution_file) = setup_process_substitution(
                        &context.shell,
                        &params,
                        kind,
                        subshell_command,
                    )?;

                    params
                        .open_files
                        .set_fd(installed_fd_num, substitution_file);

                    args.push(CommandArg::String(std::format!(
                        "/dev/fd/{installed_fd_num}"
                    )));
                }
                CommandPrefixOrSuffixItem::AssignmentWord(assignment, word) => {
                    if args.is_empty() {
                        // If we haven't yet seen any arguments, then this must be a proper
                        // scoped assignment. Add it to the list we're accumulating.
                        assignments.push(assignment);
                    } else {
                        if command_takes_assignments {
                            // This looks like an assignment, and the command being invoked is a
                            // well-known builtin that takes arguments that need to function like
                            // assignments (but which are processed by the builtin).
                            let expanded =
                                expand_assignment(&mut context.shell, &params, assignment).await?;
                            args.push(CommandArg::Assignment(expanded));
                        } else {
                            // This *looks* like an assignment, but it's really a string we should
                            // fully treat as a regular looking
                            // argument.
                            let mut next_args = expansion::full_expand_and_split_word(
                                &mut context.shell,
                                &params,
                                word,
                            )
                            .await?
                            .into_iter()
                            .map(CommandArg::String)
                            .collect();
                            args.append(&mut next_args);
                        }
                    }
                }
                CommandPrefixOrSuffixItem::Word(arg) => {
                    let mut next_args =
                        expansion::full_expand_and_split_word(&mut context.shell, &params, arg)
                            .await?;

                    if args.is_empty() {
                        if let Some(cmd_name) = next_args.first() {
                            if let Some(alias_value) =
                                context.shell.aliases().get(cmd_name.as_str())
                            {
                                //
                                // TODO(#57): This is a total hack; aliases are supposed to be
                                // handled much earlier in the process.
                                //
                                let mut alias_pieces: Vec<_> = alias_value
                                    .split_ascii_whitespace()
                                    .map(|i| i.to_owned())
                                    .collect();

                                next_args.remove(0);
                                alias_pieces.append(&mut next_args);

                                next_args = alias_pieces;
                            }

                            let first_arg = next_args[0].as_str();

                            // Check if we're going to be invoking a special declaration builtin.
                            // That will change how we parse and process args.
                            if context
                                .shell
                                .builtins()
                                .get(first_arg)
                                .is_some_and(|r| !r.disabled && r.declaration_builtin)
                            {
                                command_takes_assignments = true;
                            }
                        }
                    }

                    let mut next_args = next_args.into_iter().map(CommandArg::String).collect();
                    args.append(&mut next_args);
                }
            }
        }

        // If we have a command, then execute it.
        if let Some(CommandArg::String(cmd_name)) = args.first().cloned() {
            let mut stderr = params.stderr(&context.shell);

            let (owned_shell, parent_shell) = match context.shell {
                commands::ShellForCommand::ParentShell(shell) => (None, shell),
                commands::ShellForCommand::OwnedShell { target, parent } => (Some(target), parent),
            };

            let shell = if let Some(owned_shell) = owned_shell {
                commands::ShellForCommand::OwnedShell {
                    target: owned_shell,
                    parent: parent_shell,
                }
            } else {
                commands::ShellForCommand::ParentShell(parent_shell)
            };

            let context = PipelineExecutionContext {
                shell,
                process_group: context.process_group,
            };

            match execute_command(context, params, cmd_name, assignments, args).await {
                Ok(result) => Ok(result),
                Err(err) => {
                    let _ = parent_shell.display_error(&mut stderr, &err);

                    let result = err.into_result(parent_shell);
                    Ok(result.into())
                }
            }
        } else {
            // No command to run; assignments must be applied to this shell.
            for assignment in assignments {
                // Apply the assignment. Don't mark as fatal - let errors be handled
                // at the program level so multiple complete_commands can execute independently.
                apply_assignment(
                    assignment,
                    &mut context.shell,
                    &params,
                    false,
                    None,
                    EnvironmentScope::Global,
                )
                .await?;
            }

            // Assignment-only statements clear $_ (set to empty string).
            // This matches bash behavior where assignments don't have a "last
            // argument".
            context.shell.update_last_arg_variable(None);

            // We need to set the last exit status to indicate assignment success,
            // but only if there was no status set during expansion. We use the
            // status count captured before expansion to detect if command
            // substitution (or other expansion) set an exit status.
            if status_change_count_before_expansion == context.shell.last_exit_status_change_count()
            {
                context.shell.set_last_exit_status(0);
            }

            // Return the last exit status we have; in some cases, an expansion
            // might result in a non-zero exit status stored in the shell.
            Ok(ExecutionResult::new(context.shell.last_exit_status()).into())
        }
    }
}

async fn execute_command(
    mut context: PipelineExecutionContext<'_, impl extensions::ShellExtensions>,
    params: ExecutionParameters,
    cmd_name: String,
    assignments: Vec<&ast::Assignment>,
    args: Vec<CommandArg>,
) -> Result<ExecutionSpawnResult, error::Error> {
    // Push a new ephemeral environment scope for the duration of the command. We'll
    // set command-scoped variable assignments after doing so, and revert them before
    // returning.
    let mut guard = crate::env::ScopeGuard::new(&mut context.shell, EnvironmentScope::Command);

    for assignment in &assignments {
        // Ensure it's tagged as exported and created in the command scope.
        apply_assignment(
            assignment,
            guard.shell(),
            &params,
            true,
            Some(EnvironmentScope::Command),
            EnvironmentScope::Command,
        )
        .await?;
    }

    if guard.shell().options().print_commands_and_arguments {
        guard
            .shell()
            .trace_command(
                &params,
                args.iter().map(|arg| arg.quote_for_tracing()).join(" "),
            )
            .await;
    }

    guard.detach();
    drop(guard);

    // Construct the command struct.
    let mut cmd = commands::SimpleCommand::new(context.shell, params, cmd_name, args);
    cmd.process_group = context.process_group;

    // Arrange to pop off that ephemeral environment scope.
    cmd.post_execute = Some(|shell| shell.env_mut().pop_scope(EnvironmentScope::Command));

    // Run through any pre-execution hooks as best effort.
    let _ = commands::on_preexecute(&mut cmd).await;

    // Execute
    // TODO(jobs): do we need to move self back to foreground on error here?
    cmd.execute().await
}

async fn expand_assignment(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    assignment: &ast::Assignment,
) -> Result<ast::Assignment, error::Error> {
    let value = expand_assignment_value(shell, params, &assignment.value).await?;
    Ok(ast::Assignment {
        name: basic_expand_assignment_name(shell, params, &assignment.name).await?,
        value,
        append: assignment.append,
        loc: assignment.loc.clone(),
    })
}

async fn basic_expand_assignment_name(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    name: &ast::AssignmentName,
) -> Result<ast::AssignmentName, error::Error> {
    match name {
        ast::AssignmentName::VariableName(name) => {
            let expanded = expansion::basic_expand_word(shell, params, name).await?;
            Ok(ast::AssignmentName::VariableName(expanded))
        }
        ast::AssignmentName::ArrayElementName(name, index) => {
            let expanded_name = expansion::basic_expand_word(shell, params, name).await?;
            let expanded_index = expansion::basic_expand_word(shell, params, index).await?;
            Ok(ast::AssignmentName::ArrayElementName(
                expanded_name,
                expanded_index,
            ))
        }
    }
}

async fn expand_assignment_value(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    value: &ast::AssignmentValue,
) -> Result<ast::AssignmentValue, error::Error> {
    let expanded = match value {
        ast::AssignmentValue::Scalar(s) => {
            let expanded_word = expansion::basic_expand_assignment_word(shell, params, s).await?;
            ast::AssignmentValue::Scalar(ast::Word::from(expanded_word))
        }
        ast::AssignmentValue::Array(arr) => {
            let mut expanded_values = vec![];
            for (key, value) in arr {
                if let Some(k) = key {
                    let expanded_key = expansion::basic_expand_assignment_word(shell, params, k)
                        .await?
                        .into();
                    let expanded_value =
                        expansion::basic_expand_assignment_word(shell, params, value)
                            .await?
                            .into();
                    expanded_values.push((Some(expanded_key), expanded_value));
                } else {
                    // Array elements are treated as regular words, not assignments
                    let split_expanded_value =
                        expansion::full_expand_and_split_word(shell, params, value).await?;
                    for expanded_value in split_expanded_value {
                        expanded_values.push((None, expanded_value.into()));
                    }
                }
            }

            ast::AssignmentValue::Array(expanded_values)
        }
    };

    Ok(expanded)
}

#[expect(clippy::too_many_lines)]
async fn apply_assignment(
    assignment: &ast::Assignment,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    mut export: bool,
    required_scope: Option<EnvironmentScope>,
    creation_scope: EnvironmentScope,
) -> Result<(), error::Error> {
    // Figure out if we are trying to assign to a variable or assign to an element of an existing
    // array.
    let mut array_index;
    let variable_name = match &assignment.name {
        ast::AssignmentName::VariableName(name) => {
            array_index = None;
            name
        }
        ast::AssignmentName::ArrayElementName(name, index) => {
            let expanded = expansion::basic_expand_word(shell, params, index).await?;
            array_index = Some(expanded);
            name
        }
    };

    // Expand the values.
    let new_value = match &assignment.value {
        ast::AssignmentValue::Scalar(unexpanded_value) => {
            let value =
                expansion::basic_expand_assignment_word(shell, params, unexpanded_value).await?;
            ShellValueLiteral::Scalar(value)
        }
        ast::AssignmentValue::Array(unexpanded_values) => {
            let mut elements = vec![];
            for (unexpanded_key, unexpanded_value) in unexpanded_values {
                let key = match unexpanded_key {
                    Some(unexpanded_key) => Some(
                        expansion::basic_expand_assignment_word(shell, params, unexpanded_key)
                            .await?,
                    ),
                    None => None,
                };

                if key.is_some() {
                    let value =
                        expansion::basic_expand_assignment_word(shell, params, unexpanded_value)
                            .await?;
                    elements.push((key, value));
                } else {
                    // Array elements are treated as regular words, not assignments
                    let values =
                        expansion::full_expand_and_split_word(shell, params, unexpanded_value)
                            .await?;
                    for value in values {
                        elements.push((None, value));
                    }
                }
            }
            ShellValueLiteral::Array(ArrayLiteral(elements))
        }
    };

    if shell.options().print_commands_and_arguments {
        let op = if assignment.append { "+=" } else { "=" };
        shell
            .trace_command(params, std::format!("{}{op}{new_value}", assignment.name))
            .await;
    }

    // See if we need to eval an array index.
    if let Some(idx) = &array_index {
        let will_be_indexed_array = if let Some((_, existing_value)) =
            shell.env().get(variable_name)
        {
            matches!(
                existing_value.value(),
                ShellValue::IndexedArray(_) | ShellValue::Unset(ShellValueUnsetType::IndexedArray)
            )
        } else {
            true
        };

        if will_be_indexed_array {
            array_index = Some(
                arithmetic::expand_and_eval(shell, params, idx.as_str(), false)
                    .await?
                    .to_string(),
            );
        }
    }

    // Read option before taking mutable borrow on env.
    let export_variables_on_modification = shell.options().export_variables_on_modification;

    // See if we can find an existing value associated with the variable.
    if let Some((existing_value_scope, existing_value)) =
        shell.env_mut().get_mut(variable_name.as_str())
    {
        if required_scope.is_none() || Some(existing_value_scope) == required_scope {
            if let Some(array_index) = array_index {
                match new_value {
                    ShellValueLiteral::Scalar(s) => {
                        existing_value.assign_at_index(array_index, s, assignment.append)?;
                    }
                    ShellValueLiteral::Array(_) => {
                        return error::unimp("replacing an array item with an array");
                    }
                }
            } else {
                if !export
                    && export_variables_on_modification
                    && !matches!(new_value, ShellValueLiteral::Array(_))
                {
                    export = true;
                }

                existing_value.assign(new_value, assignment.append)?;
            }

            if export {
                existing_value.export();
            }

            // That's it!
            return Ok(());
        }
    }

    // If we fell down here, then we need to add it.
    let new_value = if let Some(array_index) = array_index {
        match new_value {
            ShellValueLiteral::Scalar(s) => {
                ShellValue::indexed_array_from_literals(ArrayLiteral(vec![(Some(array_index), s)]))
            }
            ShellValueLiteral::Array(_) => {
                return error::unimp("cannot assign list to array member");
            }
        }
    } else {
        match new_value {
            ShellValueLiteral::Scalar(s) => {
                export = export || shell.options().export_variables_on_modification;
                ShellValue::String(s)
            }
            ShellValueLiteral::Array(values) => ShellValue::indexed_array_from_literals(values),
        }
    };

    let mut new_var = ShellVariable::new(new_value);

    if export {
        new_var.export();
    }

    shell.env_mut().add(variable_name, new_var, creation_scope)
}

#[expect(clippy::too_many_lines)]
pub(crate) async fn setup_redirect(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &'_ mut ExecutionParameters,
    redirect: &ast::IoRedirect,
) -> Result<(), error::Error> {
    match redirect {
        ast::IoRedirect::OutputAndError(f, append) => {
            let mut expanded_fields =
                expansion::full_expand_and_split_word(shell, params, f).await?;
            if expanded_fields.len() != 1 {
                return Err(error::ErrorKind::InvalidRedirection.into());
            }

            let expanded_file_path = expanded_fields.remove(0);
            setup_redirect_output_and_error_to(shell, params, &expanded_file_path, *append)?;
        }

        ast::IoRedirect::File(specified_fd_num, kind, target) => {
            match target {
                ast::IoFileRedirectTarget::Filename(f) => {
                    let mut options = std::fs::File::options();

                    let mut expanded_fields =
                        expansion::full_expand_and_split_word(shell, params, f).await?;

                    if expanded_fields.len() != 1 {
                        return Err(error::ErrorKind::InvalidRedirection.into());
                    }

                    let expanded_file_path: PathBuf =
                        shell.absolute_path(Path::new(expanded_fields.remove(0).as_str()));

                    let default_fd_if_unspecified = get_default_fd_for_redirect_kind(kind);
                    let mut creation = None;

                    match kind {
                        ast::IoFileRedirectKind::Read => {
                            options.read(true);
                        }
                        ast::IoFileRedirectKind::Write => {
                            let no_clobber = shell
                                .options()
                                .disallow_overwriting_regular_files_via_output_redirection;
                            let existing_regular_file = expanded_file_path.is_file();

                            options.write(true);

                            if !no_clobber || !existing_regular_file {
                                options.truncate(true);
                            }

                            creation = Some(RedirectCreation {
                                mode: shell.redirect_creation_mode(),
                                exclusive: no_clobber && existing_regular_file,
                            });
                        }
                        ast::IoFileRedirectKind::Append => {
                            options.append(true);
                            creation = Some(RedirectCreation {
                                mode: shell.redirect_creation_mode(),
                                exclusive: false,
                            });
                        }
                        ast::IoFileRedirectKind::ReadAndWrite => {
                            options.read(true);
                            options.write(true);
                            creation = Some(RedirectCreation {
                                mode: shell.redirect_creation_mode(),
                                exclusive: false,
                            });
                        }
                        ast::IoFileRedirectKind::Clobber => {
                            options.write(true);
                            options.truncate(true);
                            creation = Some(RedirectCreation {
                                mode: shell.redirect_creation_mode(),
                                exclusive: false,
                            });
                        }
                        ast::IoFileRedirectKind::DuplicateInput => {
                            options.read(true);
                        }
                        ast::IoFileRedirectKind::DuplicateOutput => {
                            options.write(true);
                            creation = Some(RedirectCreation {
                                mode: shell.redirect_creation_mode(),
                                exclusive: false,
                            });
                        }
                    }

                    let fd_num = specified_fd_num.unwrap_or(default_fd_if_unspecified);

                    let opened_file = shell
                        .open_redirect_file(&options, &expanded_file_path, params, creation)
                        .map_err(|err| {
                            error::ErrorKind::RedirectionFailure(
                                expanded_file_path.to_string_lossy().to_string(),
                                err.to_string(),
                            )
                        })?;

                    params.open_files.set_fd(fd_num, opened_file);
                }

                ast::IoFileRedirectTarget::Fd(fd) => {
                    let default_fd_if_unspecified = match kind {
                        ast::IoFileRedirectKind::DuplicateInput => 0,
                        ast::IoFileRedirectKind::DuplicateOutput => 1,
                        _ => {
                            return error::unimp("unexpected redirect kind");
                        }
                    };

                    let fd_num = specified_fd_num.unwrap_or(default_fd_if_unspecified);

                    if let Some(f) = params.try_fd(shell, *fd) {
                        let target_file = f.try_clone()?;

                        params.open_files.set_fd(fd_num, target_file);
                    } else {
                        return Err(error::ErrorKind::BadFileDescriptor(*fd).into());
                    }
                }

                ast::IoFileRedirectTarget::Duplicate(word) => {
                    let default_fd_if_unspecified = match kind {
                        ast::IoFileRedirectKind::DuplicateInput => 0,
                        ast::IoFileRedirectKind::DuplicateOutput => 1,
                        _ => {
                            return error::unimp("unexpected redirect kind");
                        }
                    };

                    let fd_num = specified_fd_num.unwrap_or(default_fd_if_unspecified);

                    let mut expanded_fields =
                        expansion::full_expand_and_split_word(shell, params, word).await?;

                    if expanded_fields.len() != 1 {
                        return Err(error::ErrorKind::InvalidRedirection.into());
                    }

                    let mut expanded = expanded_fields.remove(0);

                    let dash = if expanded.ends_with('-') {
                        expanded.pop();
                        true
                    } else {
                        false
                    };

                    if expanded.is_empty() {
                        // Nothing to do
                    } else if expanded.chars().all(|c: char| c.is_ascii_digit()) {
                        let source_fd_num = expanded
                            .parse::<ShellFd>()
                            .map_err(|_| error::ErrorKind::InvalidRedirection)?;

                        // Duplicate the fd.
                        let target_file = if let Some(f) = params.try_fd(shell, source_fd_num) {
                            f.try_clone()?
                        } else {
                            return Err(error::ErrorKind::BadFileDescriptor(source_fd_num).into());
                        };

                        params.open_files.set_fd(fd_num, target_file);
                    } else if fd_num == 1 && !dash {
                        // Special case for compatibility: redirect stdout and stderr to the file
                        // given by `expanded`.
                        setup_redirect_output_and_error_to(
                            shell, params, &expanded, false, /* append? */
                        )?;
                    } else {
                        return Err(error::ErrorKind::InvalidRedirection.into());
                    }

                    if dash {
                        // Close the specified fd. Ignore it if it's not valid.
                        params.open_files.remove_fd(fd_num);
                    }
                }

                ast::IoFileRedirectTarget::ProcessSubstitution(substitution_kind, subshell_cmd) => {
                    match kind {
                        ast::IoFileRedirectKind::Read
                        | ast::IoFileRedirectKind::Write
                        | ast::IoFileRedirectKind::Append
                        | ast::IoFileRedirectKind::ReadAndWrite
                        | ast::IoFileRedirectKind::Clobber => {
                            let (substitution_fd, substitution_file) = setup_process_substitution(
                                shell,
                                params,
                                substitution_kind,
                                subshell_cmd,
                            )?;

                            let target_file = substitution_file.try_clone()?;
                            params.open_files.set_fd(substitution_fd, substitution_file);

                            let fd_num = specified_fd_num
                                .unwrap_or_else(|| get_default_fd_for_redirect_kind(kind));

                            params.open_files.set_fd(fd_num, target_file);
                        }
                        _ => return error::unimp("invalid process substitution"),
                    }
                }
            }
        }

        ast::IoRedirect::HereDocument(fd_num, io_here) => {
            // If not specified, default to stdin (fd 0).
            let fd_num = fd_num.unwrap_or(0);

            // Expand if required.
            let io_here_doc = if io_here.requires_expansion {
                expansion::basic_expand_heredoc_word(shell, params, &io_here.doc).await?
            } else {
                io_here.doc.flatten()
            };

            let f = setup_open_file_with_contents(io_here_doc.as_str())?;

            params.open_files.set_fd(fd_num, f);
        }

        ast::IoRedirect::HereString(fd_num, word) => {
            // If not specified, default to stdin (fd 0).
            let fd_num = fd_num.unwrap_or(0);

            let mut expanded_word = expansion::basic_expand_word(shell, params, word).await?;
            expanded_word.push('\n');

            let f = setup_open_file_with_contents(expanded_word.as_str())?;

            params.open_files.set_fd(fd_num, f);
        }
    }

    Ok(())
}

/// Sets up redirection of both stdout and stderr to the same file, given by `file_path`.
///
/// # Arguments
///
/// * `shell` - The shell instance.
/// * `params` - The execution parameters to modify.
/// * `file_path` - The path to the file to redirect output and error to.
/// * `append` - Whether to append. If `false`, the file will be truncated.
fn setup_redirect_output_and_error_to(
    shell: &Shell<impl extensions::ShellExtensions>,
    params: &mut ExecutionParameters,
    file_path: &str,
    append: bool,
) -> Result<(), error::Error> {
    let abs_file_path: PathBuf = shell.absolute_path(Path::new(file_path));

    let mut file_options = std::fs::File::options();
    file_options.write(true).truncate(!append).append(append);

    let creation = RedirectCreation {
        mode: shell.redirect_creation_mode(),
        exclusive: false,
    };

    let stdout_file = shell
        .open_redirect_file(&file_options, &abs_file_path, params, Some(creation))
        .map_err(|err| {
            error::ErrorKind::RedirectionFailure(
                abs_file_path.to_string_lossy().to_string(),
                err.to_string(),
            )
        })?;

    let stderr_file = stdout_file.try_clone()?;

    params.open_files.set_fd(OpenFiles::STDOUT_FD, stdout_file);
    params.open_files.set_fd(OpenFiles::STDERR_FD, stderr_file);

    Ok(())
}

/// Returns whether the given descriptor is one of the standard stream descriptors.
const fn is_standard_fd(fd: ShellFd) -> bool {
    matches!(
        fd,
        openfiles::OpenFiles::STDIN_FD
            | openfiles::OpenFiles::STDOUT_FD
            | openfiles::OpenFiles::STDERR_FD
    )
}

const fn get_default_fd_for_redirect_kind(kind: &ast::IoFileRedirectKind) -> ShellFd {
    match kind {
        ast::IoFileRedirectKind::Read => 0,
        ast::IoFileRedirectKind::Write => 1,
        ast::IoFileRedirectKind::Append => 1,
        ast::IoFileRedirectKind::ReadAndWrite => 0,
        ast::IoFileRedirectKind::Clobber => 1,
        ast::IoFileRedirectKind::DuplicateInput => 0,
        ast::IoFileRedirectKind::DuplicateOutput => 1,
    }
}

fn setup_process_substitution(
    shell: &Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    kind: &ast::ProcessSubstitutionKind,
    subshell_cmd: &ast::SubshellCommand,
) -> Result<(ShellFd, OpenFile), error::Error> {
    // TODO(execute): Don't execute synchronously!
    // Execute in a subshell.
    let mut subshell = shell.clone();

    // Set up execution parameters for the child execution.
    let mut child_params = params.clone();
    child_params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;

    // Set up pipe so we can connect to the command.
    let (reader, writer) = std::io::pipe()?;
    let (reader, writer) = (reader.into(), writer.into());

    let target_file = match kind {
        ast::ProcessSubstitutionKind::Read => {
            child_params.open_files.set_fd(OpenFiles::STDOUT_FD, writer);
            reader
        }
        ast::ProcessSubstitutionKind::Write => {
            child_params.open_files.set_fd(OpenFiles::STDIN_FD, reader);
            writer
        }
    };

    // Asynchronously spawn off the subshell; we intentionally don't block on its
    // completion.
    let subshell_cmd = subshell_cmd.to_owned();
    tokio::spawn(async move {
        // Intentionally ignore the result of the subshell command.
        let _ = subshell_cmd
            .list
            .execute(&mut subshell, &child_params)
            .await;
    });

    // Starting at 63 (a.k.a. 64-1)--and decrementing--look for an
    // available fd.
    let mut candidate_fd_num = 63;
    while params.open_files.contains_fd(candidate_fd_num) {
        candidate_fd_num -= 1;
        if candidate_fd_num == 0 {
            return error::unimp("no available file descriptors");
        }
    }

    Ok((candidate_fd_num, target_file))
}

fn setup_open_file_with_contents(contents: &str) -> Result<OpenFile, error::Error> {
    let (reader, mut writer) = std::io::pipe()?;

    let bytes = contents.as_bytes();

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::fd::AsFd as _;

        let len = i32::try_from(bytes.len())
            .map_err(|_err| error::Error::from(error::ErrorKind::TooMuchData))?;
        nix::fcntl::fcntl(reader.as_fd(), nix::fcntl::FcntlArg::F_SETPIPE_SZ(len))?;
    }

    writer.write_all(bytes)?;
    drop(writer);

    Ok(reader.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_stage_exec_replacement_is_consumed() {
        let mut result = ExecutionResult::new(0);
        result.next_control_flow = crate::ExecutionControlFlow::ExecReplaced;

        let consumed = consume_exec_replaced_result(result);

        assert!(consumed.is_normal_flow());
        assert!(consumed.is_success());
    }

    #[test]
    fn unrelated_control_flow_is_preserved() {
        let mut result = ExecutionResult::new(7);
        result.next_control_flow = crate::ExecutionControlFlow::ExitShell;

        let preserved = consume_exec_replaced_result(result);

        assert!(matches!(
            preserved.next_control_flow,
            crate::ExecutionControlFlow::ExitShell
        ));
        assert_eq!(u8::from(preserved.exit_code), 7);
    }

    /// Records the pipeline observer calls made by the interpreter.
    #[derive(Default)]
    struct RecordingPipelineObserver {
        begins: std::sync::Mutex<Vec<(Option<u64>, usize, bool, bool)>>,
        labels: std::sync::Mutex<Vec<(u64, usize, String)>>,
        pipes: std::sync::Mutex<Vec<(u64, usize, String, String)>>,
        stages: std::sync::Mutex<Vec<(u64, usize, u8, Option<i32>)>>,
        finishes: std::sync::Mutex<Vec<(u64, u8)>>,
        units: std::sync::Mutex<Vec<Option<u64>>>,
        commands: std::sync::Mutex<Vec<(u64, Option<u64>, usize, usize)>>,
        ends: std::sync::Mutex<Vec<(u64, Option<u8>)>>,
    }

    impl PipelineObserver for RecordingPipelineObserver {
        fn parsed(
            &self,
            _program: &ast::Program,
            _source: Option<&str>,
            parent: Option<u64>,
        ) -> Option<u64> {
            let mut units = self.units.lock().unwrap();
            units.push(parent);
            Some(units.len() as u64)
        }

        fn command_begin(
            &self,
            unit: Option<u64>,
            command: &ast::Command,
            _parent: Option<u64>,
        ) -> Option<u64> {
            let span = command.location()?;
            let mut commands = self.commands.lock().unwrap();
            let id = commands.len() as u64 + 1;
            commands.push((id, unit, span.start.index, span.end.index));
            Some(id)
        }

        fn command_end(&self, execution: u64, status: Option<u8>, _signal: Option<i32>) {
            self.ends.lock().unwrap().push((execution, status));
        }

        fn begin(
            &self,
            parent: Option<u64>,
            stages: usize,
            bang: bool,
            pipefail: bool,
        ) -> std::io::Result<u64> {
            let mut begins = self.begins.lock().unwrap();
            begins.push((parent, stages, bang, pipefail));
            Ok(u64::try_from(begins.len()).unwrap())
        }

        fn label(&self, id: u64, stage: usize, command: &str) -> std::io::Result<()> {
            self.labels
                .lock()
                .unwrap()
                .push((id, stage, command.to_owned()));
            Ok(())
        }

        fn pipe(
            &self,
            id: u64,
            edge: usize,
            producer: &str,
            consumer: &str,
        ) -> std::io::Result<Option<(OpenFile, OpenFile)>> {
            self.pipes
                .lock()
                .unwrap()
                .push((id, edge, producer.to_owned(), consumer.to_owned()));
            Ok(None)
        }

        fn stage_done(
            &self,
            id: u64,
            stage: usize,
            status: u8,
            signal: Option<i32>,
        ) -> std::io::Result<()> {
            self.stages
                .lock()
                .unwrap()
                .push((id, stage, status, signal));
            Ok(())
        }

        fn finish(&self, id: u64, status: u8) -> std::io::Result<()> {
            self.finishes.lock().unwrap().push((id, status));
            Ok(())
        }
    }

    async fn observing_shell() -> (
        crate::Shell,
        ExecutionParameters,
        Arc<RecordingPipelineObserver>,
    ) {
        let shell = crate::Shell::builder()
            .profile(crate::ProfileLoadBehavior::Skip)
            .rc(crate::RcLoadBehavior::Skip)
            .build()
            .await
            .expect("shell should build");

        let observer = Arc::new(RecordingPipelineObserver::default());

        let mut params = shell.default_exec_params();
        params.set_pipeline_observer(observer.clone());
        params.set_fd(
            openfiles::OpenFiles::STDIN_FD,
            openfiles::null().expect("null stdin"),
        );
        params.set_fd(
            openfiles::OpenFiles::STDOUT_FD,
            openfiles::null().expect("null stdout"),
        );
        params.set_fd(
            openfiles::OpenFiles::STDERR_FD,
            openfiles::null().expect("null stderr"),
        );

        (shell, params, observer)
    }

    #[tokio::test]
    async fn command_observation_tracks_function_source_and_real_completion() {
        let (mut shell, params, observer) = observing_shell().await;
        let source = "f() { /bin/echo ok; }; f; f; /bin/sh -c 'exit 7'";
        let result = shell
            .run_string(source, &crate::SourceInfo::default(), &params)
            .await
            .unwrap();
        assert_eq!(u8::from(result.exit_code), 7);
        let commands = observer.commands.lock().unwrap();
        let ends = observer.ends.lock().unwrap();
        assert_eq!(commands.len(), ends.len());
        assert!(commands.iter().all(|(_, unit, _, _)| *unit == Some(1)));
        assert_eq!(
            commands
                .iter()
                .filter(|(_, _, start, _)| *start == 6)
                .count(),
            2
        );
        assert_eq!(ends.last().unwrap().1, Some(7));
        drop(ends);
        drop(commands);
        let mut unobserved = params.clone();
        unobserved.pipeline_observer = None;
        let result = shell
            .run_string(source, &crate::SourceInfo::default(), &unobserved)
            .await
            .unwrap();
        assert_eq!(u8::from(result.exit_code), 7);
    }

    #[tokio::test]
    async fn pipeline_occurrence_reports_structure_and_lifecycle() {
        let (mut shell, params, observer) = observing_shell().await;

        let result = shell
            .run_string(
                "echo a | echo b | echo c",
                &crate::SourceInfo::default(),
                &params,
            )
            .await
            .expect("pipeline should run");
        assert!(result.is_success());

        assert_eq!(
            *observer.begins.lock().unwrap(),
            vec![(None, 3, false, false)]
        );
        assert_eq!(
            *observer.labels.lock().unwrap(),
            vec![
                (1, 1, String::from("echo a")),
                (1, 2, String::from("echo b")),
                (1, 3, String::from("echo c")),
            ]
        );
        // Edges are reported in ascending order and connect stage e to e + 1.
        assert_eq!(
            *observer.pipes.lock().unwrap(),
            vec![
                (1, 1, String::from("echo a"), String::from("echo b")),
                (1, 2, String::from("echo b"), String::from("echo c")),
            ]
        );
        assert_eq!(
            *observer.stages.lock().unwrap(),
            vec![(1, 1, 0, None), (1, 2, 0, None), (1, 3, 0, None)]
        );
        assert_eq!(*observer.finishes.lock().unwrap(), vec![(1, 0)]);
    }

    #[tokio::test]
    async fn pipeline_occurrence_finishes_after_bang_inversion() {
        let (mut shell, params, observer) = observing_shell().await;

        let result = shell
            .run_string("! echo a | echo b", &crate::SourceInfo::default(), &params)
            .await
            .expect("pipeline should run");

        assert_eq!(u8::from(&result.exit_code), 1);
        assert_eq!(
            *observer.begins.lock().unwrap(),
            vec![(None, 2, true, false)]
        );
        // The final status is the negated pipeline status, not a stage status.
        assert_eq!(*observer.finishes.lock().unwrap(), vec![(1, 1)]);
    }

    #[tokio::test]
    async fn nested_pipeline_inherits_nearest_multi_stage_parent() {
        let (mut shell, params, observer) = observing_shell().await;

        shell
            .run_string(
                "echo a | { echo b | echo c; }",
                &crate::SourceInfo::default(),
                &params,
            )
            .await
            .expect("pipeline should run");

        let begins = observer.begins.lock().unwrap();
        assert_eq!(begins.len(), 2);
        assert_eq!(begins[0], (None, 2, false, false));
        assert_eq!(begins[1], (Some(1), 2, false, false));

        let stages = observer.stages.lock().unwrap();
        assert_eq!(stages.iter().filter(|(id, ..)| *id == 1).count(), 2);
        assert_eq!(stages.iter().filter(|(id, ..)| *id == 2).count(), 2);
    }

    #[tokio::test]
    async fn pipeline_failed_background_runs_once_and_finishes_observation() {
        let (mut shell, mut params, observer) = observing_shell().await;

        let stderr_file = tempfile::NamedTempFile::new().expect("create stderr file");
        let stderr_writer = stderr_file.reopen().expect("reopen stderr file");
        params.set_fd(openfiles::OpenFiles::STDERR_FD, stderr_writer.into());

        let script = "pp_missing_background_zzz | pp_missing_background_yyy &";
        let result = shell
            .run_string(script, &crate::SourceInfo::default(), &params)
            .await
            .expect("background pipeline should not fail");
        assert!(result.is_success());

        // The already-finished pipeline is registered as a completed job; the
        // logical fallback must not have run it a second time.
        let job_result = shell
            .jobs_mut()
            .current_job_mut()
            .expect("background job should be registered")
            .wait()
            .await
            .expect("background job should be waitable");
        assert_eq!(u8::from(&job_result.exit_code), 127);

        let diagnostics = std::fs::read_to_string(stderr_file.path()).expect("read stderr");
        assert_eq!(
            diagnostics.matches("pp_missing_background_zzz").count(),
            1,
            "pipeline must not be executed twice: {diagnostics}"
        );
        assert_eq!(
            diagnostics.matches("pp_missing_background_yyy").count(),
            1,
            "pipeline must not be executed twice: {diagnostics}"
        );

        assert_eq!(observer.begins.lock().unwrap().len(), 1);
        assert_eq!(*observer.finishes.lock().unwrap(), vec![(1, 127)]);
    }

    #[test]
    fn single_stage_pipeline_does_not_create_an_occurrence() {
        let pipeline = parse_test_pipeline("echo alone");
        let observer = Arc::new(RecordingPipelineObserver::default());
        let params = ExecutionParameters::default();

        // Single-stage pipelines are wrappers: they report nothing on their own.
        assert_eq!(pipeline.seq.len(), 1);
        assert!(observer.begins.lock().unwrap().is_empty());
        assert!(params.pipeline_parent().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn external_signal_is_preserved_before_exit_code_conversion() {
        use std::os::unix::process::ExitStatusExt as _;

        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(13),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };

        let result = ExecutionResult::from(output);

        assert_eq!(u8::from(&result.exit_code), 141);
        assert_eq!(result.signal, Some(13));
    }

    #[cfg(unix)]
    #[test]
    fn normal_exit_has_no_signal() {
        use std::os::unix::process::ExitStatusExt as _;

        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(7 << 8),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };

        let result = ExecutionResult::from(output);

        assert_eq!(u8::from(&result.exit_code), 7);
        assert_eq!(result.signal, None);
    }

    fn parse_test_pipeline(script: &str) -> ast::Pipeline {
        let mut parser =
            brush_parser::Parser::new(script.as_bytes(), &brush_parser::ParserOptions::default());
        let program = parser.parse_program().expect("script should parse");
        let complete = program
            .complete_commands
            .into_iter()
            .next()
            .expect("one complete command");
        complete
            .0
            .into_iter()
            .next()
            .expect("one command item")
            .0
            .first
    }
}
