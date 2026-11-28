//! Startup phase timing, disabled by default.
//!
//! With `PP_PERF_TRACE=1` each sampling boundary writes one `phase=<name> micros=<n>` line to
//! stderr, for reproducible release-build baselines; when unset it produces no output and no
//! allocation beyond a single environment check. Only the environment check and the start anchor
//! live here (`OnceLock` carries no business state), and nothing is written to the
//! `~/.pp/logs` diagnostic log contract.

use std::{
    io::{self, Write},
    sync::OnceLock,
    time::Instant,
};

const TRACE_ENV: &str = "PP_PERF_TRACE";

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var_os(TRACE_ENV).is_some_and(|value| value.as_os_str() == "1"))
}

/// Process start anchor: the first statement in `main` records `mark("start")`, and every later
/// `micros` value is elapsed time relative to that point.
fn start() -> &'static Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now)
}

/// Record one phase boundary; a repeated phase name writes one line per call in call order, never
/// aggregated.
pub fn mark(phase: &'static str) {
    if !enabled() {
        return;
    }
    let micros = start().elapsed().as_micros();
    let _ = writeln!(io::stderr().lock(), "phase={phase} micros={micros}");
}
