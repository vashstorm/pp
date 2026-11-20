use anyhow::{Result, bail};
use pp_config::config::PermissionMode;
use pp_runtime::agent::Agent;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PermissionCommand {
    Status,
    Select(PermissionMode),
    Cycle,
    Rules,
    Revoke(String),
}

impl PermissionCommand {
    pub(crate) fn parse(argument: &str) -> Result<Self> {
        let words = argument.split_whitespace().collect::<Vec<_>>();
        Ok(match words.as_slice() {
            [] => Self::Status,
            ["rules"] => Self::Rules,
            ["revoke", id] => Self::Revoke((*id).into()),
            [mode] => Self::Select(mode.parse()?),
            _ => bail!("usage: /permissions [always-ask|accept-edits|yolo|rules|revoke <id>]"),
        })
    }

    pub(crate) fn apply(self, agent: &mut Agent) -> Result<Option<String>> {
        match self {
            Self::Cycle => {
                agent.set_permission_mode(None)?;
                Ok(agent.sandbox_status().warning())
            }
            Self::Select(mode) => {
                agent.set_permission_mode(Some(mode))?;
                Ok(Some(status(agent)))
            }
            Self::Status => Ok(Some(status(agent))),
            Self::Rules => agent.list_permission_rules().map(Some),
            Self::Revoke(id) => {
                agent.revoke_permission_rule(&id)?;
                Ok(Some("Saved permission revoked.".into()))
            }
        }
    }
}

fn status(agent: &Agent) -> String {
    format!(
        "Permission mode: {}\n{}",
        agent.permission_mode().as_str(),
        agent.sandbox_status().summary()
    )
}
