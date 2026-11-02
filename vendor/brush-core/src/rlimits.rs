//! Per-shell resource limit virtualization.
//!
//! `ulimit` in an embedded shell must not modify the hosting process's limits.
//! Overrides are therefore kept as shell state and only applied to child
//! processes through `pre_exec`. Reads fall back to the host's current limits,
//! which is a read-only query and never mutates process-global state.

use std::collections::HashMap;
use std::io;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use rlimit::Resource;

/// A set of process resource-limit overrides belonging to one logical shell.
#[derive(Clone, Debug, Default)]
pub struct ResourceLimits {
    /// Overrides for individual resources; absent entries report host limits.
    overrides: HashMap<Resource, (u64, u64)>,
}

impl ResourceLimits {
    /// Returns a new, empty set of overrides.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the effective `(soft, hard)` limit for the given resource.
    ///
    /// Overridden resources report the shell's own values; everything else is
    /// read from the host process without modifying it.
    pub fn get(&self, resource: Resource) -> io::Result<(u64, u64)> {
        if let Some(&limits) = self.overrides.get(&resource) {
            Ok(limits)
        } else {
            resource.get()
        }
    }

    /// Records a new soft and hard limit for the given resource.
    ///
    /// The values are validated against the privilege rules `setrlimit` would
    /// enforce (soft must not exceed hard; the hard limit must not be raised
    /// above the currently effective hard limit) but are not applied to the
    /// host process.
    pub fn set(&mut self, resource: Resource, soft: u64, hard: u64) -> io::Result<()> {
        if !resource.is_supported() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported resource",
            ));
        }

        if soft != rlimit::INFINITY && hard != rlimit::INFINITY && soft > hard {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "soft limit exceeds hard limit",
            ));
        }

        let (_, current_hard) = self.get(resource)?;
        if current_hard != rlimit::INFINITY {
            let exceeds = |value: u64| value != rlimit::INFINITY && value > current_hard;
            if exceeds(hard) || exceeds(soft) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "operation not permitted",
                ));
            }
        }

        let _ = self.overrides.insert(resource, (soft, hard));
        Ok(())
    }

    /// Returns whether the given resource has a shell-local override.
    pub fn is_overridden(&self, resource: Resource) -> bool {
        self.overrides.contains_key(&resource)
    }

    /// Iterates over all shell-local overrides.
    pub fn iter_overrides(&self) -> impl Iterator<Item = (Resource, (u64, u64))> + '_ {
        self.overrides
            .iter()
            .map(|(resource, limits)| (*resource, *limits))
    }

    /// Discards all overrides, returning to the host's limits.
    pub fn clear(&mut self) {
        self.overrides.clear();
    }

    /// Applies the given override to a command's child process.
    ///
    /// This must only be called before the command is spawned and from a
    /// context where installing a `pre_exec` hook is permitted.
    #[cfg(unix)]
    pub(crate) fn add_pre_exec_hook(&self, command: &mut std::process::Command) {
        if self.overrides.is_empty() {
            return;
        }

        let overrides: Vec<(Resource, u64, u64)> = self
            .iter_overrides()
            .map(|(resource, (soft, hard))| (resource, soft, hard))
            .collect();

        // SAFETY:
        // The closure only performs `setrlimit` calls on pre-collected values;
        // it allocates nothing, takes no locks and touches no shell state.
        unsafe {
            command.pre_exec(move || {
                for &(resource, soft, hard) in &overrides {
                    rlimit::setrlimit(resource, soft, hard)?;
                }
                Ok(())
            });
        }
    }
}
