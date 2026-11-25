mod portable;
mod to_completion;
mod to_response;

use super::Session;
use anyhow::{Result, bail};
#[cfg(test)]
use async_openai::types::{chat as c, responses as r};
#[cfg(test)]
use portable::{PortableItem, easy_message, response_item};
use pp_config::config::EditMode;
use pp_config::models::Protocol;
#[cfg(test)]
use std::borrow::Cow;
#[cfg(test)]
use to_completion::completion_message;

const UNSUPPORTED: &str = "cannot switch model: unsupported history item";

pub(super) fn convert(
    session: &Session,
    model: String,
    protocol: Protocol,
    edit_mode: EditMode,
) -> Result<Session> {
    let mut converted = match (session, protocol) {
        (Session::Completion(source), Protocol::Response) => {
            to_response::from_completion(source, model, edit_mode)
        }
        (Session::Completion(_), Protocol::Completion) => bail!(UNSUPPORTED),
        (Session::Response(source), Protocol::Response) => {
            to_response::from_response(source, model, edit_mode)
        }
        (Session::Response(source), Protocol::Completion) => {
            to_completion::from_response(source, model, edit_mode)
        }
    }?;
    if let Some(key) = session.prompt_cache_key() {
        converted.set_prompt_cache_key(key.to_owned());
    }
    Ok(converted)
}

#[cfg(test)]
mod tests;
