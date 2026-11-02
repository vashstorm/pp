//! Process supervision interfaces for embedded execution.
//!
//! When the shell runs embedded inside a host process, the host owns child
//! process supervision: it registers spawned children, reaps them, tracks
//! process-group membership and enforces stop deadlines. This module defines
//! the synchronous interface through which the shell hands children to such a
//! supervisor. No Tokio signal or process driver is required on the shell side.

#![allow(clippy::module_name_repetitions)]

use std::process::ExitStatus;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

/// A spawned child process together with the process group that owns it.
///
/// The group handle is a registration lease: the supervisor keeps the group's
/// leader as an unreaped anchor for as long as any lease exists. Leases taken
/// only to assemble a pipeline are released by the shell as soon as the whole
/// pipeline has been spawned.
pub struct SpawnedProcess {
    /// The managed child process.
    pub process: Arc<dyn ManagedProcess>,
    /// The process group the child was placed in.
    pub group: Arc<dyn ManagedGroup>,
}

/// How a child process should be assigned to a process group.
pub enum SpawnGroup {
    /// Create a new process group led by this child.
    New,
    /// Join an existing, already registered process group.
    Join(Arc<dyn ManagedGroup>),
}

/// A child process whose lifetime and exit status are managed by a supervisor.
pub trait ManagedProcess: Send + Sync {
    /// Returns the child's real process ID.
    fn pid(&self) -> i32;

    /// Returns the child's real process group ID.
    fn pgid(&self) -> i32;

    /// Polls for the child's exit status without blocking the calling thread.
    ///
    /// The supervisor reaps the child itself; this only reports the cached
    /// terminal status once the child has been waited for.
    fn poll_exit(&self, cx: &mut Context<'_>) -> Poll<std::io::Result<ExitStatus>>;
}

/// A process group whose membership is tracked by a supervisor.
pub trait ManagedGroup: Send + Sync {
    /// Returns the group's process group ID.
    fn pgid(&self) -> i32;

    /// Returns a registry-unique generation key. Keys are never reused, so a
    /// stale key can never denote a different group.
    fn key(&self) -> u64;
}

/// Permission for the shell to start short-lived cleanup children while it is
/// being stopped.
///
/// Permits are created and revoked by the supervisor; the shell only inspects
/// them and forwards them when spawning a cleanup child.
pub trait CleanupPermit: Send + Sync {
    /// Returns the instant at which the permit expires.
    fn deadline(&self) -> Instant;

    /// Returns whether the permit has already been revoked.
    fn is_revoked(&self) -> bool;
}

/// Spawns, tracks, signals, and reaps child processes on behalf of the shell.
pub trait ProcessSupervisor: Send + Sync {
    /// Spawns the given command in a new or existing process group.
    ///
    /// `cleanup` is provided only when the shell is executing a cancellation
    /// cleanup trap and the supervisor has issued a permit for it.
    fn spawn(
        &self,
        command: std::process::Command,
        group: SpawnGroup,
        cleanup: Option<&dyn CleanupPermit>,
    ) -> std::io::Result<SpawnedProcess>;

    /// Sends a raw signal number to a process ID.
    fn signal_pid(&self, pid: i32, signal: i32) -> std::io::Result<()>;

    /// Sends a raw signal number to a registered process group.
    fn signal_group(&self, group: &dyn ManagedGroup, signal: i32) -> std::io::Result<()>;

    /// Issues a cleanup permit for the current stop, if one is available.
    fn cleanup_permit(&self) -> Option<Arc<dyn CleanupPermit>>;
}
