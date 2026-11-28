//! `ask` branch: interactive questions and answer validation.

use super::*;

impl Tools {
    pub(in crate::tools) fn run_ask(
        &self,
        arguments: &str,
        call_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        let handler = self
            .ask_handler
            .as_ref()
            .context("ask requires interactive TUI mode")?;
        let args = std::sync::Arc::new(pp_core::interaction::AskArgs::parse(arguments)?);
        let answers = handler(
            pp_core::interaction::AskRequest {
                call_id: call_id.to_owned(),
                args: args.clone(),
            },
            cancellation,
        )?;
        pp_core::cancellation::check(cancellation)?;
        args.validate_answers(&answers)?;
        Ok(pp_core::interaction::answer_result(
            args.canonicalize_answers(answers),
        ))
    }
}
