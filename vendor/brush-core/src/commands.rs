//! Command execution

use std::{
    borrow::Cow,
    ffi::OsStr,
    fmt::Display,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use brush_parser::ast;
use itertools::Itertools;
use std::io::Write as _;
use sys::commands::{CommandExt, CommandFdInjectionExt, CommandFgControlExt};

use crate::{
    ErrorKind, ExecutionControlFlow, ExecutionExitCode, ExecutionParameters, ExecutionResult,
    Shell, ShellFd, builtins, commands, env, error, escape,
    extensions::{self, ShellExtensions},
    functions,
    interp::{self, Execute, ProcessGroupPolicy},
    openfiles::{self, OpenFile, OpenFiles},
    pathsearch, processes,
    processes::ProcessGroupRef,
    results::{ExecutionSpawnResult, ExecutionWaitResult},
    supervisor::{ManagedGroup, ProcessSupervisor, SpawnGroup},
    sys, trace_categories, traps, variables,
};

/// Encapsulates the result of waiting for a command to complete.
pub enum CommandWaitResult {
    /// The command completed.
    CommandCompleted(ExecutionResult),
    /// The command was stopped before it completed.
    CommandStopped(ExecutionResult, processes::ChildProcess),
}

/// Represents the context for executing a command.
pub struct ExecutionContext<'a, SE: ShellExtensions = extensions::DefaultShellExtensions> {
    /// The shell in which the command is being executed.
    pub shell: &'a mut Shell<SE>,
    /// The name of the command being executed.
    pub command_name: String,
    /// The parameters for the execution.
    pub params: ExecutionParameters,
}

impl<SE: ShellExtensions> ExecutionContext<'_, SE> {
    /// Returns the standard input file; usable with `write!` et al.
    pub fn stdin(&self) -> impl std::io::Read + 'static {
        self.params.stdin(self.shell)
    }

    /// Returns the standard output file; usable with `write!` et al.
    pub fn stdout(&self) -> impl std::io::Write + 'static {
        self.params.stdout(self.shell)
    }

    /// Returns the standard error file; usable with `write!` et al.
    pub fn stderr(&self) -> impl std::io::Write + 'static {
        self.params.stderr(self.shell)
    }

    /// Returns the file descriptor with the given number. Returns `None`
    /// if the file descriptor is not open.
    ///
    /// # Arguments
    ///
    /// * `fd` - The file descriptor number to retrieve.
    pub fn try_fd(&self, fd: ShellFd) -> Option<openfiles::OpenFile> {
        self.params.try_fd(self.shell, fd)
    }

    /// Iterates over all open file descriptors.
    pub fn iter_fds(&self) -> impl Iterator<Item = (ShellFd, openfiles::OpenFile)> {
        self.params.iter_fds(self.shell)
    }
}

/// An argument to a command.
#[derive(Clone, Debug)]
pub enum CommandArg {
    /// A simple string argument.
    String(String),
    /// An assignment/declaration; typically treated as a string, but will
    /// be specially handled by a limited set of built-in commands.
    Assignment(ast::Assignment),
}

impl Display for CommandArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::String(s) => f.write_str(s),
            Self::Assignment(a) => write!(f, "{a}"),
        }
    }
}

impl From<String> for CommandArg {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<&String> for CommandArg {
    fn from(value: &String) -> Self {
        Self::String(value.clone())
    }
}

impl CommandArg {
    pub(crate) fn quote_for_tracing(&self) -> Cow<'_, str> {
        match self {
            Self::String(s) => escape::quote_if_needed(s, escape::QuoteMode::SingleQuote),
            Self::Assignment(a) => {
                let mut s = a.name.to_string();
                let op = if a.append { "+=" } else { "=" };
                s.push_str(op);
                s.push_str(&escape::quote_if_needed(
                    a.value.to_string().as_str(),
                    escape::QuoteMode::SingleQuote,
                ));
                s.into()
            }
        }
    }
}

/// Encapsulates a possibly-owned reference to a `Shell` for command execution.
pub enum ShellForCommand<'a, SE: extensions::ShellExtensions> {
    /// The command is run in the same shell as its parent; the provided
    /// mutable reference allows modifying the parent shell.
    ParentShell(&'a mut Shell<SE>),
    /// The command is run in its own owned shell (which is also provided).
    OwnedShell {
        /// The owned shell.
        target: Box<Shell<SE>>,
        /// The parent shell.
        parent: &'a mut Shell<SE>,
    },
}

impl<SE: extensions::ShellExtensions> std::ops::Deref for ShellForCommand<'_, SE> {
    type Target = Shell<SE>;

    fn deref(&self) -> &Self::Target {
        match self {
            ShellForCommand::ParentShell(shell) => shell,
            ShellForCommand::OwnedShell { target, .. } => target,
        }
    }
}

impl<SE: extensions::ShellExtensions> std::ops::DerefMut for ShellForCommand<'_, SE> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            ShellForCommand::ParentShell(shell) => shell,
            ShellForCommand::OwnedShell { target, .. } => target,
        }
    }
}

/// Composes a `std::process::Command` to execute the given command. Appropriately
/// configures the command name and arguments, redirections, injected file
/// descriptors, environment variables, etc.
///
/// # Arguments
///
/// * `context` - The execution context in which the command is being composed.
/// * `command_name` - The name of the command to execute.
/// * `argv0` - The value to use for `argv[0]` (may be different from the command).
/// * `args` - The arguments to pass to the command.
/// * `empty_env` - If true, the command will be executed with an empty environment; if false, the
///   command will inherit environment variables marked as exported in the provided `Shell`.
#[allow(unused_variables, reason = "argv0 is only used on unix platforms")]
pub fn compose_std_command<S: AsRef<OsStr>, SE: extensions::ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
    command_name: &str,
    argv0: &str,
    args: &[S],
    empty_env: bool,
) -> Result<std::process::Command, error::Error> {
    let mut cmd = std::process::Command::new(command_name);

    // Override argv[0].
    // NOTE: Not supported on all platforms.
    cmd.arg0(argv0);

    // Pass through args.
    cmd.args(args);

    // Use the shell's current working dir.
    cmd.current_dir(context.shell.working_dir());

    // Start with a clear environment.
    cmd.env_clear();

    // Add in exported variables.
    if !empty_env {
        for (k, v) in context.shell.env().iter_exported() {
            // NOTE: To match bash behavior, we only include exported variables
            // that are set (i.e., have a value). This means a variable that
            // shows up in `declare -p` but has no *set* value will be omitted.
            if v.value().is_set() {
                cmd.env(k.as_str(), v.value().to_cow_str(context.shell).as_ref());
            }
        }
        // Set _ to the resolved command path for external commands.
        cmd.env("_", command_name);
    }

    // Add in exported functions.
    if !empty_env {
        for (func_name, registration) in context.shell.funcs().iter() {
            if registration.is_exported() {
                let var_name = std::format!("BASH_FUNC_{func_name}%%");
                let value = std::format!("() {}", registration.definition().body);
                cmd.env(var_name, value);
            }
        }
    }

    // In embedded mode there is no meaningful host stdio to inherit: a logical
    // descriptor that is unset (or still points at the host's own stream) must
    // become /dev/null rather than silently reaching the embedding process.
    let embedded = context.shell.is_embedded();

    // Redirect stdin, if applicable.
    match context.try_fd(OpenFiles::STDIN_FD) {
        Some(OpenFile::Stdin(_)) | None => {
            if embedded {
                cmd.stdin(Stdio::null());
            }
        }
        Some(stdin_file) => {
            let as_stdio: Stdio = stdin_file.into();
            cmd.stdin(as_stdio);
        }
    }

    // Redirect stdout, if applicable.
    match context.try_fd(OpenFiles::STDOUT_FD) {
        Some(OpenFile::Stdout(_)) | None => {
            if embedded {
                cmd.stdout(Stdio::null());
            }
        }
        Some(stdout_file) => {
            let as_stdio: Stdio = stdout_file.into();
            cmd.stdout(as_stdio);
        }
    }

    // Redirect stderr, if applicable.
    match context.try_fd(OpenFiles::STDERR_FD) {
        Some(OpenFile::Stderr(_)) | None => {
            if embedded {
                cmd.stderr(Stdio::null());
            }
        }
        Some(stderr_file) => {
            let as_stdio: Stdio = stderr_file.into();
            cmd.stderr(as_stdio);
        }
    }

    // Inject any other fds.
    let other_files = context.iter_fds().filter(|(fd, _)| {
        *fd != OpenFiles::STDIN_FD && *fd != OpenFiles::STDOUT_FD && *fd != OpenFiles::STDERR_FD
    });
    cmd.inject_fds(other_files)?;

    Ok(cmd)
}

pub(crate) async fn on_preexecute(
    cmd: &mut commands::SimpleCommand<'_, impl extensions::ShellExtensions>,
) -> Result<(), error::Error> {
    // Set BASH_COMMAND before invoking the DEBUG trap (and generally before
    // executing commands).
    let full_cmd = cmd.args.iter().map(|arg| arg.to_string()).join(" ");
    cmd.shell.env_mut().update_or_add(
        "BASH_COMMAND",
        variables::ShellValueLiteral::Scalar(full_cmd),
        |_| Ok(()),
        env::EnvironmentLookup::Anywhere,
        env::EnvironmentScope::Global,
    )?;

    // Fire the DEBUG trap if one is registered.
    if cmd.shell.traps().handles(traps::TrapSignal::Debug) {
        let _ = cmd
            .shell
            .invoke_trap_handler(traps::TrapSignal::Debug, &cmd.params)
            .await?;
    }

    Ok(())
}

/// Represents a simple command to be executed.
pub struct SimpleCommand<'a, SE: extensions::ShellExtensions> {
    /// The shell to run the command in.
    shell: ShellForCommand<'a, SE>,

    /// The execution parameters for the command.
    pub params: ExecutionParameters,

    /// The name of the command to execute.
    pub command_name: String,

    /// The arguments to the command, including the command itself.
    pub args: Vec<CommandArg>,

    /// Whether to consider shell functions when looking up the command name.
    /// If true, shell functions will be checked; if false, they will be ignored.
    pub use_functions: bool,

    /// Optional list of directories to search for external commands. If left
    /// `None`, the default search logic will be used.
    pub path_dirs: Option<Vec<PathBuf>>,

    /// The process group that externally executed commands should join. This may
    /// be `None`, in which case the default behavior will be used.
    pub process_group: Option<ProcessGroupRef>,

    /// Optional override for the `argv[0]` value presented to an externally
    /// spawned process. When `None`, `command_name` is used.
    pub argv0: Option<String>,

    /// Optionally provides a function that can run after execution occurs. Note
    /// that it is *not* invoked if the shell is discarded during the execution
    /// process.
    #[allow(clippy::type_complexity)]
    pub post_execute: Option<fn(&mut Shell<SE>) -> Result<(), error::Error>>,
}

impl<'a, SE: extensions::ShellExtensions> SimpleCommand<'a, SE> {
    /// Creates a new `SimpleCommand` instance.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell in which to execute the command.
    /// * `params` - The execution parameters for the command.
    /// * `command_name` - The name of the command to execute.
    /// * `args` - The arguments to the command, including the command itself.
    pub const fn new(
        shell: ShellForCommand<'a, SE>,
        params: ExecutionParameters,
        command_name: String,
        args: Vec<CommandArg>,
    ) -> Self {
        Self {
            shell,
            params,
            command_name,
            args,
            use_functions: true,
            path_dirs: None,
            process_group: None,
            argv0: None,
            post_execute: None,
        }
    }

    /// Executes the simple command.
    ///
    /// The command may be a builtin, a shell function, or an externally
    /// executed command. This function's implementation is responsible for
    /// dispatching it appropriately according to the context provided.
    #[allow(
        clippy::missing_panics_doc,
        reason = "these unwrap calls should not panic"
    )]
    pub async fn execute(mut self) -> Result<ExecutionSpawnResult, error::Error> {
        // First see if it's the name of a builtin.
        let builtin = self.shell.builtins().get(&self.command_name).cloned();

        // If we're in POSIX mode and found a special builtin (that's not disabled), then invoke it
        // without considering functions.
        if self.shell.options().posix_mode
            && builtin
                .as_ref()
                .is_some_and(|r| !r.disabled && r.special_builtin)
        {
            #[allow(clippy::unwrap_used, reason = "we just checked that builtin is Some")]
            let builtin = builtin.unwrap();
            self.params.observe_resolved("builtin");
            return self.execute_via_builtin(builtin).await;
        }

        // Assuming we weren't requested not to do so, check if it's the name of
        // a shell function.
        if self.use_functions {
            if let Some(func_registration) =
                self.shell.funcs().get(self.command_name.as_str()).cloned()
            {
                self.params.observe_resolved("function");
                return self.execute_via_function(func_registration).await;
            }
        }

        // If we haven't yet resolved the command name and found a builtin that's not disabled,
        // then invoke it.
        if let Some(builtin) = builtin {
            if !builtin.disabled {
                self.params.observe_resolved("builtin");
                return self.execute_via_builtin(builtin).await;
            }
        }

        self.params.observe_resolved("external");
        // We still haven't found a command to invoke. We'll need to look for an external command.
        if !sys::fs::contains_path_separator(&self.command_name) {
            // All else failed; if we were given path directories to search, try to look through
            // them for a matching executable. Otherwise, use our default search logic.
            let path = if let Some(path_dirs) = &self.path_dirs {
                pathsearch::search_for_executable(path_dirs.iter(), self.command_name.as_str())
                    .next()
            } else {
                self.shell
                    .find_first_executable_in_path_using_cache(&self.command_name)
            };

            if let Some(path) = path {
                self.execute_via_external(&path)
            } else {
                // Bash updates $_ even when the command is not found, so mirror
                // that here before reporting the error.
                let last_arg = Self::take_last_arg(&self.args);
                self.shell.update_last_arg_variable(last_arg);

                if let Some(post_execute) = self.post_execute {
                    let _ = post_execute(&mut self.shell);
                }

                Err(ErrorKind::CommandNotFound(self.command_name).into())
            }
        } else {
            let command_name = PathBuf::from(self.command_name.clone());
            self.execute_via_external(command_name.as_path())
        }
    }

    /// Extracts the owned string representation of the last argument of a
    /// command, suitable for recording into `$_`.
    fn take_last_arg(args: &[CommandArg]) -> Option<String> {
        args.last().map(ToString::to_string)
    }

    async fn execute_via_builtin(
        self,
        builtin: builtins::Registration<SE>,
    ) -> Result<ExecutionSpawnResult, error::Error> {
        match self.shell {
            ShellForCommand::OwnedShell { target, .. } => {
                Ok(Self::execute_via_builtin_in_owned_shell(
                    *target,
                    self.params,
                    builtin,
                    self.command_name,
                    self.args,
                ))
            }
            ShellForCommand::ParentShell(..) => {
                self.execute_via_builtin_in_parent_shell(builtin).await
            }
        }
    }

    fn execute_via_builtin_in_owned_shell(
        mut shell: Shell<SE>,
        params: ExecutionParameters,
        builtin: builtins::Registration<SE>,
        command_name: String,
        args: Vec<CommandArg>,
    ) -> ExecutionSpawnResult {
        let last_arg = Self::take_last_arg(&args);
        let join_handle = tokio::task::spawn_blocking(move || {
            let cmd_context = ExecutionContext {
                shell: &mut shell,
                command_name,
                params,
            };

            let rt = tokio::runtime::Handle::current();
            let result = rt.block_on(execute_builtin_command(&builtin, cmd_context, args));

            // Update $_ after command execution.
            shell.update_last_arg_variable(last_arg);

            result
        });

        ExecutionSpawnResult::StartedTask(join_handle)
    }

    async fn execute_via_builtin_in_parent_shell(
        self,
        builtin: builtins::Registration<SE>,
    ) -> Result<ExecutionSpawnResult, error::Error> {
        let mut shell = self.shell;
        let last_arg = Self::take_last_arg(&self.args);

        let cmd_context = ExecutionContext {
            shell: &mut shell,
            command_name: self.command_name,
            params: self.params,
        };

        let result = execute_builtin_command(&builtin, cmd_context, self.args).await;

        // Update $_ after command execution.
        shell.update_last_arg_variable(last_arg);

        if let Some(post_execute) = self.post_execute {
            let _ = post_execute(&mut shell);
        }

        let result = result?;

        Ok(result.into())
    }

    async fn execute_via_function(
        self,
        func_registration: functions::Registration,
    ) -> Result<ExecutionSpawnResult, error::Error> {
        let Self {
            shell,
            params,
            command_name,
            args,
            post_execute,
            ..
        } = self;

        match shell {
            ShellForCommand::OwnedShell { target, .. } => {
                // A function invoked by an owned pipeline stage is a logical
                // process too: run it concurrently so the remaining stages can
                // be assembled while the body still produces output (or blocks
                // waiting for a sibling stage). Awaiting it here would delay
                // every later stage until the function finished.
                let handle = tokio::spawn(async move {
                    let mut shell = *target;
                    let last_arg = Self::take_last_arg(&args);
                    let cmd_context = ExecutionContext {
                        shell: &mut shell,
                        command_name,
                        params,
                    };

                    // Strip the function name off args.
                    let result =
                        invoke_shell_function(func_registration, cmd_context, &args[1..]).await?;

                    let result = match result.wait().await? {
                        ExecutionWaitResult::Completed(result) => result,
                        ExecutionWaitResult::Stopped(_) => ExecutionResult::stopped(),
                    };

                    shell.update_last_arg_variable(last_arg);

                    if let Some(post_execute) = post_execute {
                        let _ = post_execute(&mut shell);
                    }

                    Ok(result)
                });

                Ok(ExecutionSpawnResult::StartedTask(handle))
            }
            ShellForCommand::ParentShell(shell) => {
                // Parent-shell invocations (for example `lastpipe`) must keep
                // mutating the caller, so they stay synchronous.
                let mut shell = shell;
                let last_arg = Self::take_last_arg(&args);

                let cmd_context = ExecutionContext {
                    shell: &mut *shell,
                    command_name,
                    params,
                };

                // Strip the function name off args.
                let result =
                    invoke_shell_function(func_registration, cmd_context, &args[1..]).await;

                // $_ is reset *after* the function body runs, to the last
                // argument of the invocation (or the function name itself if
                // zero args). Any mutations made inside the body are
                // overwritten — this matches bash, where the caller observes
                // only the invocation's last argument.
                shell.update_last_arg_variable(last_arg);

                if let Some(post_execute) = post_execute {
                    let _ = post_execute(&mut shell);
                }

                result
            }
        }
    }

    fn execute_via_external(self, path: &Path) -> Result<ExecutionSpawnResult, error::Error> {
        let mut shell = self.shell;
        let last_arg = Self::take_last_arg(&self.args);

        let cmd_context = ExecutionContext {
            shell: &mut shell,
            command_name: self.command_name,
            params: self.params,
        };

        let resolved_path = path.to_string_lossy();
        let result = execute_external_command(
            cmd_context,
            resolved_path.as_ref(),
            self.process_group,
            self.argv0.as_deref(),
            &self.args[1..],
        );

        // Update $_ after command execution.
        shell.update_last_arg_variable(last_arg);

        if let Some(post_execute) = self.post_execute {
            let _ = post_execute(&mut shell);
        }

        result
    }
}

/// Resolves the process supervisor to use for external commands.
///
/// A supervisor installed on the execution parameters takes precedence over the
/// one installed on the shell.
pub(crate) fn resolve_supervisor<'a, SE: extensions::ShellExtensions>(
    shell: &'a Shell<SE>,
    params: &'a ExecutionParameters,
) -> Option<&'a Arc<dyn ProcessSupervisor>> {
    params
        .process_supervisor()
        .or_else(|| shell.process_supervisor())
}

/// Maps a spawn failure to the error the shell reports.
fn mapped_spawn_error<SE: extensions::ShellExtensions>(
    shell: &Shell<SE>,
    command_name: &str,
    spawn_err: std::io::Error,
) -> error::Error {
    if spawn_err.kind() == std::io::ErrorKind::NotFound {
        if !shell.working_dir().exists() {
            error::ErrorKind::WorkingDirMissing(shell.working_dir().to_owned()).into()
        } else {
            error::ErrorKind::CommandNotFound(command_name.to_owned()).into()
        }
    } else {
        error::ErrorKind::FailedToExecuteCommand(command_name.to_owned(), spawn_err).into()
    }
}

/// Applies the shell's virtual process attributes to a child command.
///
/// This covers the virtual umask, the shell's resource-limit overrides, the
/// default `SIGPIPE` disposition and any signals the shell explicitly ignores.
#[cfg(unix)]
fn apply_child_process_attributes<SE: extensions::ShellExtensions>(
    shell: &Shell<SE>,
    cmd: &mut std::process::Command,
) {
    let umask = shell
        .umask()
        .map(|mask| libc::mode_t::try_from(mask).unwrap_or(0o022));

    let ignored_signals: Vec<i32> = shell
        .traps()
        .iter_handlers()
        .filter(|(_, handler)| handler.command.trim().is_empty())
        .filter_map(|(signal, _)| i32::try_from(signal).ok())
        .filter(|signal| *signal > 0)
        .collect();

    // SAFETY:
    // The hook runs between fork and exec. It only calls async-signal-safe libc
    // functions on pre-collected values; it allocates nothing, takes no locks and
    // touches no shell state.
    unsafe {
        cmd.pre_exec(move || {
            // Rust ignores SIGPIPE in its own process; children must not inherit
            // that, or pipelines like `yes | head` would never terminate.
            //
            // SAFETY: `signal` is async-signal-safe and both arguments are constants.
            if libc::signal(libc::SIGPIPE, libc::SIG_DFL) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }

            if let Some(mask) = umask {
                libc::umask(mask);
            }

            for signal in &ignored_signals {
                libc::signal(*signal, libc::SIG_IGN);
            }

            Ok(())
        });
    }

    shell.resource_limits().add_pre_exec_hook(cmd);
}

/// Applies the shell's virtual process attributes to a child command.
#[cfg(not(unix))]
fn apply_child_process_attributes<SE: extensions::ShellExtensions>(
    _shell: &Shell<SE>,
    _cmd: &mut std::process::Command,
) {
}

/// Spawns a composed command, through the supervisor when one is installed.
///
/// An embedded shell fails closed rather than starting an unsupervised child.
fn spawn_external_command<SE: extensions::ShellExtensions>(
    shell: &Shell<SE>,
    params: &ExecutionParameters,
    mut cmd: std::process::Command,
    command_name: &str,
    new_process_group: bool,
    join_group: Option<Arc<dyn ManagedGroup>>,
    join_pgid: Option<i32>,
) -> Result<ExecutionSpawnResult, error::Error> {
    if let Some(supervisor) = resolve_supervisor(shell, params).cloned() {
        // After cancellation, only a live cleanup permit may start children.
        if params.is_cancellation_requested()
            && !params
                .cleanup_permit()
                .is_some_and(|permit| !permit.is_revoked())
        {
            return Err(error::ErrorKind::ExecutionCancelled.into());
        }

        apply_child_process_attributes(shell, &mut cmd);

        let group = join_group.map_or(SpawnGroup::New, SpawnGroup::Join);
        let permit = params.cleanup_permit().map(std::convert::AsRef::as_ref);

        let spawned = supervisor
            .spawn(cmd, group, permit)
            .map_err(|spawn_err| mapped_spawn_error(shell, command_name, spawn_err))?;

        return Ok(ExecutionSpawnResult::StartedProcess(
            processes::ChildProcess::from_spawned(spawned),
        ));
    }

    if shell.is_embedded() {
        return Err(error::ErrorKind::ProcessSupervisorRequired.into());
    }

    match sys::process::spawn(cmd) {
        Ok(child) => {
            // Retrieve the pid.
            #[expect(clippy::cast_possible_wrap)]
            let pid = child.id().map(|id| id as i32);

            let mut actual_pgid = join_pgid;
            if let Some(pid) = &pid {
                if new_process_group {
                    actual_pgid = Some(*pid);
                }
            } else {
                tracing::warn!("could not retrieve pid for child process");
            }

            Ok(ExecutionSpawnResult::StartedProcess(
                processes::ChildProcess::new(child, pid, actual_pgid),
            ))
        }
        Err(spawn_err) => Err(mapped_spawn_error(shell, command_name, spawn_err)),
    }
}

pub(crate) fn execute_external_command(
    context: ExecutionContext<'_, impl extensions::ShellExtensions>,
    executable_path: &str,
    process_group: Option<ProcessGroupRef>,
    argv0_override: Option<&str>,
    args: &[CommandArg],
) -> Result<ExecutionSpawnResult, error::Error> {
    // Filter out the args; we only want strings.
    let cmd_args = args
        .iter()
        .filter_map(|e| {
            if let CommandArg::String(s) = e {
                Some(s)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    // Before we lose ownership of the open files, figure out if stdin will be a terminal.
    let child_stdin_is_terminal = context
        .try_fd(openfiles::OpenFiles::STDIN_FD)
        .is_some_and(|f| f.is_terminal());

    // Figure out if we should be setting up a new process group.
    let new_pg = matches!(
        context.params.process_group_policy,
        ProcessGroupPolicy::NewProcessGroup
    );

    // Compose the std::process::Command that encapsulates what we want to launch.
    // argv[0] defaults to context.command_name (the user-facing name of the
    // command) unless the caller specified an explicit override.
    let argv0 = argv0_override.unwrap_or(context.command_name.as_str());
    #[allow(unused_mut, reason = "only mutated on unix platforms")]
    let mut cmd = compose_std_command(
        &context,
        executable_path,
        argv0,
        cmd_args.as_slice(),
        false, /* empty environment? */
    )?;

    // Supervised children get their process group from the supervisor, which
    // never inherits the host's group; only the unsupervised path configures the
    // group here.
    if resolve_supervisor(context.shell, &context.params).is_none() {
        // Set up process group state.
        if new_pg {
            // Check if we'll be doing terminal control setup (which includes setsid)
            if child_stdin_is_terminal && context.shell.options().external_cmd_leads_session {
                // Don't set process_group(0) - setsid() in pre_exec will handle it
                cmd.lead_session();
            } else {
                // Normal case: create new process group in current session
                cmd.process_group(0);
                if child_stdin_is_terminal {
                    cmd.take_foreground();
                }
            }
        } else {
            // We need to join an established process group.
            if let Some(pgid) = process_group.as_ref().map(|group| group.pgid) {
                cmd.process_group(pgid);
            }
        }
    }

    // When tracing is enabled, report.
    tracing::debug!(
        target: trace_categories::COMMANDS,
        "Spawning: cmd='{} {}'",
        cmd.get_program().to_string_lossy().to_string(),
        cmd.get_args()
            .map(|a| a.to_string_lossy().to_string())
            .join(" ")
    );

    let join_pgid = process_group.as_ref().map(|group| group.pgid);
    let join_group = process_group.and_then(|group| group.managed);

    let result = spawn_external_command(
        context.shell,
        &context.params,
        cmd,
        context.command_name.as_str(),
        new_pg,
        join_group,
        join_pgid,
    );

    if result.is_err() && context.shell.options().interactive && !context.shell.is_embedded() {
        sys::terminal::move_self_to_foreground()?;
    }

    result
}

/// Builds the result reported when an `exec` target cannot be started.
///
/// Unless `execfail` is enabled, a failed `exec` ends a non-interactive shell, so
/// the failure is reported as an `ExitShell` transition carrying the status.
fn exec_start_failure<SE: extensions::ShellExtensions>(
    shell: &Shell<SE>,
    exit_code: ExecutionExitCode,
) -> ExecutionResult {
    let mut result = ExecutionResult::from(exit_code);

    // As in bash, a failed `exec` ends a non-interactive shell unless `execfail`
    // is enabled; interactive shells keep running.
    if !shell.options().exit_on_exec_fail && !shell.options().interactive {
        result.next_control_flow = ExecutionControlFlow::ExitShell;
    }

    result
}

/// Runs an `exec` target as a managed child and ends the logical shell.
///
/// The program is resolved through the shell's `PATH` (unless it contains a path
/// separator) and spawned through the same supervised path as any other external
/// command. While the child runs no trap is dispatched: a successful `exec`
/// logically replaced the shell, so its `TERM` and `EXIT` handlers no longer
/// apply. The transition reported on success is [`ExecutionControlFlow::ExecReplaced`],
/// which propagates out of functions and loops and is consumed by the innermost
/// subshell boundary.
///
/// # Arguments
///
/// * `context` - The execution context whose shell is being replaced.
/// * `program` - The program to execute, as written.
/// * `argv0` - The `argv[0]` value to present to the program.
/// * `args` - Arguments following the program.
/// * `empty_env` - Whether to execute with an empty environment.
pub async fn run_exec_command<SE: extensions::ShellExtensions>(
    context: ExecutionContext<'_, SE>,
    program: &str,
    argv0: &str,
    args: &[String],
    empty_env: bool,
) -> Result<ExecutionResult, error::Error> {
    let ExecutionContext {
        shell,
        params,
        command_name: _command_name,
    } = context;

    let resolved = if sys::fs::contains_path_separator(program) {
        PathBuf::from(program)
    } else {
        match shell.find_first_executable_in_path_using_cache(program) {
            Some(path) => path,
            None => {
                writeln!(params.stderr(shell), "{program}: command not found")?;
                return Ok(exec_start_failure(
                    shell,
                    ExecutionExitCode::NotFound,
                ));
            }
        }
    };

    let resolved_path = resolved.to_string_lossy();
    let compose_context = ExecutionContext {
        shell: &mut *shell,
        command_name: program.to_owned(),
        params: params.clone(),
    };

    let cmd = compose_std_command(
        &compose_context,
        resolved_path.as_ref(),
        argv0,
        args,
        empty_env,
    )?;

    match spawn_external_command(
        shell,
        &params,
        cmd,
        program,
        false,
        None,
        None,
    ) {
        Ok(ExecutionSpawnResult::StartedProcess(mut child)) => {
            shell.mark_exec_replaced();

            // This child no longer needs an assembly lease on its group.
            child.release_group_lease();

            let wait_result = child.wait().await?;

            let mut result = match wait_result {
                processes::ProcessWaitResult::Completed(output) => {
                    ExecutionResult::from(output)
                }
                processes::ProcessWaitResult::Stopped => ExecutionResult::stopped(),
            };

            result.next_control_flow = ExecutionControlFlow::ExecReplaced;

            Ok(result)
        }
        Ok(ExecutionSpawnResult::Completed(_)) | Ok(ExecutionSpawnResult::StartedTask(_)) => {
            Ok(exec_start_failure(shell, ExecutionExitCode::CannotExecute))
        }
        Err(err) => {
            let exit_code = match err.kind() {
                error::ErrorKind::CommandNotFound(..) => ExecutionExitCode::NotFound,
                _ => ExecutionExitCode::CannotExecute,
            };

            let _ = writeln!(params.stderr(shell), "{program}: {err}");

            Ok(exec_start_failure(shell, exit_code))
        }
    }
}

async fn execute_builtin_command<SE: extensions::ShellExtensions>(
    builtin: &builtins::Registration<SE>,
    context: ExecutionContext<'_, SE>,
    args: Vec<CommandArg>,
) -> Result<ExecutionResult, error::Error> {
    // In POSIX mode, special builtins that return errors are to be treated as fatal.
    let mark_errors_fatal = builtin.special_builtin && context.shell.options().posix_mode;

    match (builtin.execute_func)(context, args).await {
        Ok(result) => Ok(result),
        Err(e) => {
            // Broken pipe errors should silently return the appropriate exit code
            if let Some(io_err) = e.as_io_error() {
                if io_err.kind() == std::io::ErrorKind::BrokenPipe {
                    return Ok(ExecutionExitCode::from(io_err).into());
                }
            }

            Err(if mark_errors_fatal { e.into_fatal() } else { e })
        }
    }
}

pub(crate) async fn invoke_shell_function(
    function: functions::Registration,
    mut context: ExecutionContext<'_, impl extensions::ShellExtensions>,
    args: &[CommandArg],
) -> Result<ExecutionSpawnResult, error::Error> {
    let ast::FunctionBody(body, redirects) = &function.definition().body;

    // Apply any redirects specified at function definition-time.
    if let Some(redirects) = redirects {
        for redirect in &redirects.0 {
            interp::setup_redirect(context.shell, &mut context.params, redirect).await?;
        }
    }

    let positional_args = args.iter().map(|a| a.to_string());

    // Pass through open files.
    let mut params = context.params.clone();
    params.observation_unit = function.observation_unit();

    if let Some(cancelled) = context.shell.check_interpreter_boundary(&params).await? {
        return Ok(cancelled.into());
    }

    // Note that we're going deeper. Once we do this, we need to make sure we don't bail early
    // before "exiting" the function.
    context.shell.enter_function(
        context.command_name.as_str(),
        &function,
        positional_args,
        &context.params,
    )?;

    // Invoke the function.
    let result = body.execute(context.shell, &params).await;

    // Clean up parameters so any owned files are closed.
    drop(params);

    // We've come back out, reflect it.
    context.shell.leave_function()?;

    // Get the actual execution result from the body of the function.
    let mut result = result?;

    // Handle control-flow.
    match result.next_control_flow {
        ExecutionControlFlow::BreakLoop { .. } | ExecutionControlFlow::ContinueLoop { .. } => {
            return error::unimp("break or continue returned from function invocation");
        }
        ExecutionControlFlow::ReturnFromFunctionOrScript => {
            // It's now been handled.
            result.next_control_flow = ExecutionControlFlow::Normal;
        }
        _ => {}
    }

    Ok(result.into())
}

pub(crate) async fn invoke_command_in_subshell_and_get_output(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    s: String,
) -> Result<String, error::Error> {
    // Instantiate a subshell to run the command in.
    let mut subshell = shell.clone();

    // Command substitutions don't inherit errexit by default. Only inherit it when
    // command_subst_inherits_errexit is enabled, otherwise disable errexit in the subshell.
    if !shell.options().command_subst_inherits_errexit {
        subshell.options_mut().exit_on_nonzero_command_exit = false;
    }

    // Get our own set of parameters we can customize and use.
    let mut params = params.clone();
    params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;

    // Set up pipe so we can read the output.
    let (reader, writer) = std::io::pipe()?;
    params.set_fd(OpenFiles::STDOUT_FD, writer.into());

    let mut async_reader = sys::async_pipe::AsyncPipeReader::new(reader)?;

    let cmd_join_handle = tokio::spawn(run_substitution_command(subshell, params, s));

    let output_str = async_reader.read_to_string().await?;

    // Now observe the command's completion.
    let run_result = cmd_join_handle.await?;
    let cmd_result = run_result?;

    // Store the status.
    shell.set_last_exit_status(cmd_result.exit_code.into());

    // Note: $_ is naturally isolated from the parent because we cloned the
    // shell to run the substitution.

    Ok(output_str)
}

async fn run_substitution_command(
    mut shell: Shell<impl extensions::ShellExtensions>,
    mut params: ExecutionParameters,
    command: String,
) -> Result<ExecutionResult, error::Error> {
    if let Some(cancelled) = shell.check_interpreter_boundary(&params).await? {
        return Ok(cancelled);
    }

    // Parse the string into a whole shell program.
    let parse_result = shell.parse_string(command);

    // Check for a command that is only an input redirection ("< file").
    // If detected, emulate `cat file` to stdout and return immediately.
    // If we failed to parse, then we'll fall below and handle it there.
    if let Ok(program) = &parse_result {
        if let Some(redir) = try_unwrap_bare_input_redir_program(program) {
            interp::setup_redirect(&mut shell, &mut params, redir).await?;
            std::io::copy(&mut params.stdin(&shell), &mut params.stdout(&shell))?;
            return Ok(ExecutionResult::new(0));
        }
    }

    // TODO(source-info): review this
    let source_info = crate::SourceInfo::from("main");

    // Handle the parse result using default shell behavior.
    shell
        .run_parsed_result(parse_result, &source_info, &params)
        .await
}

// Detects a subshell command that consists solely of a single input redirection
// (e.g., "< file"), returning the IoRedirect when present.
fn try_unwrap_bare_input_redir_program(program: &ast::Program) -> Option<&ast::IoRedirect> {
    // We're looking for exactly one complete command...
    let [complete] = program.complete_commands.as_slice() else {
        return None;
    };

    // ...a single list item...
    let ast::CompoundList(items) = complete;
    let [item] = items.as_slice() else {
        return None;
    };

    // ...with a single pipeline (no && or || chaining)...
    let and_or = &item.0;
    if !and_or.additional.is_empty() {
        return None;
    }

    // ...not negated...
    let pipeline = &and_or.first;
    if pipeline.bang {
        return None;
    }

    // ...with a single command in the pipeline...
    let [ast::Command::Simple(simple_cmd)] = pipeline.seq.as_slice() else {
        return None;
    };

    // ...with no program word/name and no suffix...
    if simple_cmd.word_or_name.is_some() || simple_cmd.suffix.is_some() {
        return None;
    }

    // ...and exactly one prefix containing an I/O redirect...
    let prefix = simple_cmd.prefix.as_ref()?;
    let [ast::CommandPrefixOrSuffixItem::IoRedirect(redir)] = prefix.0.as_slice() else {
        return None;
    };

    // ...that is a file input redirection to a filename, targeting stdin.
    match redir {
        ast::IoRedirect::File(
            fd,
            ast::IoFileRedirectKind::Read,
            ast::IoFileRedirectTarget::Filename(..),
        ) if fd.is_none_or(|fd| fd == openfiles::OpenFiles::STDIN_FD) => Some(redir),
        _ => None,
    }
}
