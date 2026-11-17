use std::{ffi::OsString, io, path::Path};

use brush_core::{CommandWrapper, OpenIntent};
use pp_sandbox::Policy;

pub(super) fn build_command_wrapper(policy: &Policy) -> io::Result<CommandWrapper> {
    let (program, args) = policy
        .command_wrapper_prefix()
        .map_err(|err| io::Error::other(format!("preparing sandbox wrapper: {err}")))?;
    Ok(CommandWrapper {
        program: OsString::from(program),
        args,
        env: vec![(
            OsString::from("PP_SANDBOX"),
            OsString::from(policy.backend()),
        )],
    })
}

pub(super) fn check_open(policy: &Policy, path: &Path, intent: OpenIntent) -> io::Result<()> {
    match intent {
        OpenIntent::Read => policy.check_read(path),
        OpenIntent::Write => policy.check_write(path),
        OpenIntent::ReadWrite => {
            policy.check_read(path)?;
            policy.check_write(path)
        }
    }
}
