//! Signal delivery on behalf of shell commands.

use crate::{error, extensions, sys};

impl<SE: extensions::ShellExtensions> crate::Shell<SE> {
    /// Sends a raw signal number to a process.
    ///
    /// Destructive signals aimed at the hosting process, PID 0, PID -1 or the
    /// host's own process group are refused, so that a mistaken `kill` cannot
    /// take down the embedding application. Signal 0 remains available as an
    /// existence probe.
    ///
    /// # Arguments
    ///
    /// * `pid` - The target process ID; negative values address a process group.
    /// * `signum` - The raw signal number to send.
    pub fn signal_process(&self, pid: i32, signum: i32) -> Result<(), error::Error> {
        Self::check_signal_target(pid, signum)?;

        // Only process IDs owned by one of this shell's jobs go through the
        // registry; signalling any other process keeps the ordinary, unsandboxed
        // `kill` semantics.
        if let Some(supervisor) = self.process_supervisor()
            && self.jobs().contains_pid(pid)
        {
            return supervisor
                .signal_pid(pid, signum)
                .map_err(|_err| error::ErrorKind::FailedToSendSignal.into());
        }

        send_signal(pid, signum)
    }

    /// Sends a raw signal number to a process group.
    ///
    /// # Arguments
    ///
    /// * `pgid` - The target process group ID.
    /// * `signum` - The raw signal number to send.
    pub fn signal_process_group(&self, pgid: i32, signum: i32) -> Result<(), error::Error> {
        Self::check_signal_target(pgid, signum)?;

        send_signal(pgid.saturating_neg(), signum)
    }

    /// Refuses destructive signals aimed at the host process or its group.
    fn check_signal_target(target: i32, signum: i32) -> Result<(), error::Error> {
        if signum == 0 {
            return Ok(());
        }

        let Ok(host_pid) = i32::try_from(std::process::id()) else {
            return Err(error::ErrorKind::RefusedToSignalHost.into());
        };

        let host_pgid = sys::terminal::get_process_group_id().unwrap_or(host_pid);

        let blocked = target == 0
            || target == -1
            || target == host_pid
            || target == -host_pid
            || target == host_pgid
            || target == -host_pgid;

        if blocked {
            return Err(error::ErrorKind::RefusedToSignalHost.into());
        }

        Ok(())
    }
}

/// Sends a raw signal to a PID or (for negative IDs) a process group.
#[cfg(unix)]
fn send_signal(pid: i32, signum: i32) -> Result<(), error::Error> {
    // SAFETY:
    // `kill` is a thin syscall wrapper whose arguments are plain scalars; it does
    // not touch memory owned by this process.
    let result = unsafe { libc::kill(pid, signum) };

    if result == 0 {
        Ok(())
    } else {
        Err(error::ErrorKind::FailedToSendSignal.into())
    }
}

/// Sends a raw signal to a PID or (for negative IDs) a process group.
#[cfg(not(unix))]
fn send_signal(_pid: i32, _signum: i32) -> Result<(), error::Error> {
    Err(error::ErrorKind::NotSupportedOnThisPlatform("signals").into())
}
