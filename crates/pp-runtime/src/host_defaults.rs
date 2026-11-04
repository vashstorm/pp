use std::sync::OnceLock;

#[derive(Clone, Copy, Debug)]
pub struct HostDefaults {
    pub(crate) umask: u32,
}

static DEFAULTS: OnceLock<HostDefaults> = OnceLock::new();

/// Captures process-global defaults before the application starts any threads.
pub fn capture() -> HostDefaults {
    // SAFETY: main calls this before argument parsing, logging, or thread creation.
    // POSIX specifies no non-mutating umask query, so restore it immediately.
    let mask = unsafe { libc::umask(0) };
    // SAFETY: same single-threaded startup boundary; restores the captured value.
    unsafe { libc::umask(mask) };
    HostDefaults {
        umask: u32::from(mask),
    }
}

pub fn install(defaults: HostDefaults) {
    DEFAULTS
        .set(defaults)
        .expect("host defaults are installed exactly once");
}

pub(crate) fn current() -> HostDefaults {
    #[cfg(test)]
    {
        HostDefaults { umask: 0o022 }
    }
    #[cfg(not(test))]
    {
        *DEFAULTS
            .get()
            .expect("host defaults must be installed at process startup")
    }
}
