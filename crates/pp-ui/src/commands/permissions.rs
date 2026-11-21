use anyhow::{Result, bail};
use pp_config::config::{PermissionMode, PermissionPolicy};
use pp_permission::BashRule;
use pp_runtime::agent::Agent;

const USAGE: &str = "usage: /permissions [always-ask|accept-edits|yolo|rules|allow <prefix...>|prompt <prefix...>|deny <prefix...>|revoke <index|rule>]";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PermissionCommand {
    Status,
    Select(PermissionMode),
    Cycle,
    Rules,
    AddBashRule(BashRule),
    Revoke(String),
}

impl PermissionCommand {
    pub(crate) const USAGE: &str = USAGE;

    pub(crate) fn parse(argument: &str) -> Result<Self> {
        let words = argument.split_whitespace().collect::<Vec<_>>();
        Ok(match words.as_slice() {
            [] => Self::Status,
            ["rules"] => Self::Rules,
            ["allow", prefix @ ..] if !prefix.is_empty() => Self::AddBashRule(BashRule {
                prefix: prefix.iter().map(|s| (*s).to_owned()).collect(),
                policy: PermissionPolicy::Allow,
            }),
            ["prompt", prefix @ ..] if !prefix.is_empty() => Self::AddBashRule(BashRule {
                prefix: prefix.iter().map(|s| (*s).to_owned()).collect(),
                policy: PermissionPolicy::Prompt,
            }),
            ["deny", prefix @ ..] if !prefix.is_empty() => Self::AddBashRule(BashRule {
                prefix: prefix.iter().map(|s| (*s).to_owned()).collect(),
                policy: PermissionPolicy::Deny,
            }),
            ["revoke", target @ ..] if !target.is_empty() => Self::Revoke(target.join(" ")),
            [mode] => Self::Select(mode.parse()?),
            _ => bail!("{USAGE}"),
        })
    }

    pub(crate) fn apply_with(
        self,
        permission: &pp_permission::Gate,
        sandbox: &pp_sandbox::Sandbox,
    ) -> Result<Option<String>> {
        match self {
            Self::Cycle => {
                permission.select(None);
                sandbox.set_mode_override(None);
                Ok(sandbox.view(permission.sandbox_mode()).status().warning())
            }
            Self::Select(mode) => {
                permission.select(Some(mode));
                sandbox.set_mode_override(None);
                Ok(Some(status_with(permission, sandbox)))
            }
            Self::Status => Ok(Some(status_with(permission, sandbox))),
            Self::Rules => permission.list_rules().map(Some),
            Self::AddBashRule(rule) => {
                let prefix = rule.prefix.join(" ");
                permission.save_bash_rule(rule)?;
                Ok(Some(format!("Saved bash permission rule: {prefix}")))
            }
            Self::Revoke(target) => {
                permission.revoke_rule(&target)?;
                Ok(Some("Saved permission revoked.".into()))
            }
        }
    }

    pub(crate) fn apply(self, agent: &mut Agent) -> Result<Option<String>> {
        let changed = matches!(self, Self::Cycle | Self::Select(_));
        let permission = agent.permission_gate();
        let sandbox = agent.sandbox();
        let output = self.apply_with(&permission, &sandbox)?;
        if changed {
            agent.sync_permission_mode()?;
        }
        Ok(output)
    }
}

fn status_with(permission: &pp_permission::Gate, sandbox: &pp_sandbox::Sandbox) -> String {
    format!(
        "Permission mode: {}\n{}",
        permission.snapshot().as_str(),
        sandbox.view(permission.sandbox_mode()).status().summary()
    )
}
